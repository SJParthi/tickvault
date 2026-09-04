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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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
    /// A price field on the tick is NON-FINITE (`NaN` / `±Inf`).
    ///
    /// Fail-closed on purpose, and this is a REAL wire shape rather than a
    /// theoretical one: `parse_quote_packet` is proven by its own test
    /// (`quote.rs::…day_open.is_nan()`) to propagate `f32::NAN` into
    /// `day_open/high/low/close`, and IDX_I moved to Quote mode on
    /// 2026-08-21, so the packet type that carries it is live.
    ///
    /// The old `opt_price` gate was `(v != 0.0).then(…)`, which **NaN
    /// passes** (NaN compares unequal to everything), and
    /// [`f32_to_f64_clean`] returns non-finite values unchanged — so a NaN
    /// reached the ILP line, QuestDB REJECTED the whole batch, the rejected
    /// buffer was rescued to the spill tier, and `tick_spill_replay` retries
    /// a permanently-unacceptable file first in every round (files are
    /// sorted, and there is no DLQ for this tier). One malformed field
    /// therefore disabled tick recovery for every hour that followed.
    /// Refusing ONE loud row is strictly better than losing the batch and
    /// wedging the recovery tier behind it.
    ///
    /// The depth path in the same drain already does this
    /// (`dhan_feed_stack.rs` — "the depth twin of `tick_price_is_sane`");
    /// the tick path did not.
    #[error(
        "tick price field `{field}` is non-finite (NaN/Inf) — row refused \
         rather than emitted as a NaN/Inf ILP value that QuestDB rejects, \
         which would spill the whole batch and wedge the replay tier"
    )]
    PriceNotFinite {
        /// Which column carried the non-finite value.
        field: &'static str,
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

        // Fail-closed on non-finite prices BEFORE any of them can reach an
        // ILP line. NaN compares unequal to everything, so the `!= 0.0`
        // gate below does NOT stop it, and `f32_to_f64_clean` passes
        // non-finite through unchanged — see `TickRowError::PriceNotFinite`
        // for the full chain this closes (rejected batch -> spilled buffer
        // -> permanently-wedged replay round).
        //
        // `0.0` stays legal: it is the documented "not carried" sentinel for
        // a Ticker (16-byte) packet and becomes NULL below. Only NaN/Inf are
        // refused. Zero-alloc: five register compares, no branch on the
        // happy path beyond the `is_finite` test itself.
        for (field, value) in [
            ("ltp", tick.last_traded_price),
            ("open", tick.day_open),
            ("high", tick.day_high),
            ("low", tick.day_low),
            ("close", tick.day_close),
        ] {
            if !value.is_finite() {
                metrics::counter!("tv_tick_rows_refused_total", "reason" => "price_not_finite")
                    .increment(1);
                error!(
                    code = ErrorCode::StorageGapTickDedupSegment.code_str(),
                    security_id = tick.security_id,
                    segment = segment_code_to_str(tick.exchange_segment_code),
                    field,
                    value = %value,
                    "tick row refused — price field is non-finite (NaN/Inf); \
                     emitting it would make QuestDB reject the whole batch, \
                     spill the rescued buffer, and wedge the replay tier \
                     behind a file it can never accept. Row NOT written."
                );
                return Err(TickRowError::PriceNotFinite { field });
            }
        }

        // A Ticker (16-byte) packet carries only LTP + LTT; Quote/Full add the
        // rest. Zero means "not carried" for those, so it becomes NULL. Every
        // value reaching here is finite (refused above), so the `!= 0.0` gate
        // now means exactly what it reads as.
        // 2026-08-25 — `is_finite()` added, and it is NOT redundant with the
        // loop above.
        //
        // That loop covers five fields; `average_traded_price` is a SIXTH
        // caller of this closure and was never in it. `NaN != 0.0` is TRUE, and
        // both `f32_to_f64_clean` and `round_to_2dp` pass non-finite straight
        // through — so a NaN ATP went to ILP. The parser proves it can: Dhan
        // Quote packets carry NaN there, asserted by
        // `parser::quote`'s own `average_traded_price.is_nan()` test.
        //
        // The consequence is exactly the chain `TickRowError::PriceNotFinite`
        // documents as CLOSED: QuestDB rejects the whole batch, `discard_pending`
        // clears up to 1,000 good rows, the rescued buffer spills, and the replay
        // tier wedges behind a file it can never accept.
        //
        // A non-finite OPTIONAL price becomes NULL and is counted, rather than
        // refusing the row the way the five mandatory prices do. Refusing here
        // would discard a tick whose LTP is perfectly good — losing a tick to
        // protect an auxiliary column, which is the wrong trade.
        let opt_price = |v: f32| {
            if !v.is_finite() {
                note_non_finite_optional_price(
                    tick.security_id,
                    segment_code_to_str(tick.exchange_segment_code),
                );
                return None;
            }
            (v != 0.0).then(|| round_to_2dp(f32_to_f64_clean(v)))
        };
        let opt_qty = |v: u32| (v != 0).then(|| i64::from(v));

        // Hoisted: the receipt time is now needed TWICE — once as its own
        // column and once as the fallback designated timestamp for a row whose
        // LTT is the vendor's never-traded sentinel.
        let received_at_ist_nanos = (tick.received_at_nanos != 0).then_some(
            tick.received_at_nanos
                .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
        );

        Ok(Self {
            security_id,
            segment: segment_code_to_str(tick.exchange_segment_code),
            ltp: round_to_2dp(f32_to_f64_clean(tick.last_traded_price)),
            volume: saturate_volume_to_i64(u64::from(tick.volume), security_id),
            // Dhan sends exchange_timestamp already in IST epoch SECONDS — no
            // +19800 offset (`.claude/rules/dhan/live-market-feed.md`). A
            // never-traded SENTINEL falls back to the receipt time — see
            // [`row_timestamp_ist_nanos`].
            ts_ist_nanos: row_timestamp_ist_nanos(tick.exchange_timestamp, received_at_ist_nanos),
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
            received_at_ist_nanos,
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

// ---------------------------------------------------------------------------
// Tick ILP spill — the rescue tier the ticks writer never had
// ---------------------------------------------------------------------------

/// Directory the failed-flush ILP payloads are appended to.
///
/// Sibling of the frame WAL (`data/ws_wal`) and the seal spill, deliberately
/// under the same `data/` root so one retention sweep can see all three.
pub const TICK_SPILL_DIR: &str = "data/spill/ticks";

/// Floor for the spill-directory ceiling, in bytes (512 MiB).
///
/// This WAS the whole ceiling, a fixed 512 MiB, and it cost 1,695,983 ticks in
/// one event on the live box on 2026-08-25. The log timeline, in full, because
/// the shape of it is the argument:
///
/// | 08:31:09 | boot #1's WAL-replay flush fails. **1,774,802 rows are
///              RESCUED, writing 544,034,728 bytes** — a single rescue that on
///              its own exceeds the 512 MiB ceiling |
/// | 08:31:56 | the deploy swaps the binary; the process restarts |
/// | 08:33:44 | boot #2's flush fails. **1,695,983 rows REFUSED** — "tick spill
///              dir at or past its 536870912-byte cap". `tv_ticks_spilled_total`
///              stays 0: no rescue, permanent loss |
///
/// **The ceiling was smaller than ONE unit of the work it existed to rescue.**
/// A WAL-replay flush is the largest single buffer this process ever holds, and
/// at ~544 MB it could not fit even once — so the first rescue consumed the
/// entire budget and the second was guaranteed to be refused. That is not a
/// tuning miss; it is a bound that could never do its job.
///
/// The volume it was protecting is **200 GB**, so the ceiling that destroyed
/// the data was **0.26% of the disk**.
///
/// The rationale for HAVING a ceiling was and remains right — QuestDB and the
/// frame WAL share this disk, and a rescue that can fill the root volume trades
/// a bounded tick loss for an unbounded outage of everything. What was wrong is
/// that the number was pinned to no machine in particular. That is a shape this
/// repository has already repaired once, for exactly this reason: the RAM-store
/// budget was a hardcoded 10 GiB "sized against the r8g.xlarge 32 GiB host"
/// until 2026-08-21, when it became a runtime fraction of the host's real
/// memory. [`tick_spill_max_bytes`] applies that same pattern here.
///
/// Retained as the FLOOR so the ceiling can never end up smaller than it was,
/// and as the fallback when the volume cannot be measured.
pub const TICK_SPILL_MIN_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Free bytes a tick-spill write must leave behind, after its own payload.
///
/// 2 GiB. The spill tier shares this volume with QuestDB and the frame WAL, so
/// a rescue that fills the disk converts a bounded tick loss into an unbounded
/// outage of every table on the box — the exact trade the ceiling exists to
/// prevent, arrived at from the other direction. Sized against the measured
/// trough: free space on the prod volume cycles down to ~1.94 GB while QuestDB
/// stages and releases files, so this refuses precisely in the window where a
/// half-gigabyte write is most dangerous.
pub const SPILL_MIN_FREE_HEADROOM_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Free-space floor that turns the spill SOFT CEILING into a refusal.
///
/// Past `tick_spill_max_bytes()` the rescue keeps writing while the disk has
/// genuine room, and refuses only once free space falls to this reserve. It is
/// deliberately EIGHT TIMES [`SPILL_MIN_FREE_HEADROOM_BYTES`]: that guard is a
/// per-write floor sized against one ~544 MB rescue landing in a momentary
/// trough, whereas this one is a standing reserve for a spill directory that
/// has already outgrown its rail and will keep growing until the drain catches
/// up. QuestDB needs room to stage and merge partitions, not merely room for
/// one more file.
///
/// 16 GiB against the live 322 GB volume is ~5% held back — an order of
/// magnitude more protection than the 2 GiB per-write floor, while still
/// leaving ~127 GB of today's free space usable for rescue instead of the
/// 9.37 GB the old total-derived rail allowed.
///
/// # Why this exists at all
///
/// MEASURED 2026-09-01: the total-derived ceiling refused 4 tick rescues and
/// 48 depth rescues with 143 GB free, permanently discarding 5,142,980 ticks
/// and 238,615,500 depth rows. A rail that fires at a fixed fraction of a
/// disk's TOTAL size fires identically on an empty disk and a full one, which
/// makes it useless as a protection and effective only as a data shredder.
pub const SPILL_SOFT_CEILING_FREE_RESERVE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Worst case bytes ONE session of ticks can spill if the database is
/// unreachable for the entire session.
///
/// DERIVED, not chosen. Measured 2026-09-01: 83,446,729 ticks ingested in a
/// session, and a `ticks` row is 144 bytes on the wire (4 SYMBOL columns at
/// 4 B of interned key + 7 eight-byte columns + the designated timestamp).
/// 83.4M x 144 B = 12.0 GB. Rounded up to 16 GiB so a busier session at
/// TODAY'S universe still fits.
///
/// # ⚠ CORRECTED 2026-09-01 (same day, adversarial review) — it does NOT
/// # cover the 25,000-instrument target, and the first version claimed it did
///
/// The sentence above originally ended "…or a universe grown toward the
/// 25,000-instrument ceiling, still fits." **That is false, by roughly 21x,
/// and it was my claim.** The 83.4M-tick session ran the deduped live set of
/// ~868 instruments. Scaling to the authorized ~24,600 is 28.3x, i.e. ~2.36
/// billion ticks x 144 B ≈ **340 GB** — larger than the entire 300 GB volume,
/// so no reserve can cover it and none should pretend to.
///
/// The number itself is NOT raised, because a reserve bigger than the disk is
/// not a reserve. What changes is the claim: this figure covers a full-session
/// database outage **at today's ~868-instrument universe**, and at the 24,600
/// target a full-session outage overflows the volume no matter how the reserve
/// is set. The protection at that scale is the shed ladder and the archival
/// tier draining the disk, not this constant.
///
/// Recorded rather than quietly narrowed because the failure mode is the one
/// this repository keeps paying for: a measurement taken at one scale, written
/// up as if it held at another, and then trusted by the next reader.
///
/// This exists only to derive [`DEPTH_SPILL_FREE_RESERVE_BYTES`] below. It is
/// deliberately a named constant rather than a literal inside that
/// expression, because the number it encodes is a MEASUREMENT and a future
/// reader must be able to see what it was measured from.
pub const WORST_CASE_SESSION_TICK_SPILL_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Per-write free-space floor for the DEPTH rescue tier.
///
/// # The gap this closes (found by adversarial review, 2026-09-01)
///
/// The tick tier has TWO independent free-space defences: the soft ceiling's
/// reserve (which engages only once the spill directory is past its size
/// rail) and [`SPILL_MIN_FREE_HEADROOM_BYTES`], a per-write floor checked on
/// EVERY rescue regardless of directory size.
///
/// The depth tier had only the first. Below its size rail it wrote with no
/// free-space consultation at all, which meant the regime it spends most of
/// its life in — a small spill directory on a disk that QuestDB is staging
/// merges onto — had no bound whatsoever. Measured free-space troughs of
/// 1.94 GB are on the record in this file; a depth rescue landing in one had
/// nothing to stop it.
///
/// # Why it is the TICK CEILING RESERVE, and not merely double the tick floor
///
/// CRITICAL DEFECT FOUND BY ADVERSARIAL REVIEW, 2026-09-01 — inside the very
/// change written to prevent it.
///
/// The first version of this constant was `2 * SPILL_MIN_FREE_HEADROOM_BYTES`
/// (4 GiB), reasoning that it should mirror the ceiling reserves' 2:1 ratio.
/// That reasoning is WRONG, because the two tiers' ceiling arms are gated by
/// INDEPENDENT directory sizes, so a ratio between the reserves only expresses
/// a priority while both tiers happen to be in the same regime. With the tick
/// spill directory OVER its size rail and the depth directory UNDER its own:
///
/// | free space | tick tier | depth tier |
/// |---|---|---|
/// | 5 GiB | ceiling arm fires, `free <= 16 GiB` reserve → **REFUSE** | `UnderCeiling`, `free >= payload + 4 GiB` → **WRITE** |
///
/// Decision-critical ticks discarded while record-only depth keeps writing —
/// the exact inversion the reserve split exists to prevent, reintroduced
/// through the OTHER of the two defences. Neither `const _` assert could see
/// it: one compared floor-to-floor and the other reserve-to-reserve, and the
/// failure is CROSS-KIND.
///
/// # The invariant, stated so it can be checked mechanically
///
/// **At every free-space value where the tick tier can refuse, the depth tier
/// must already have refused.**
///
/// The tick tier refuses at `free <= SPILL_SOFT_CEILING_FREE_RESERVE_BYTES`
/// (ceiling arm) or at `free < payload + SPILL_MIN_FREE_HEADROOM_BYTES`
/// (floor). Setting this floor to the tick tier's CEILING reserve dominates
/// both — on every write, regardless of either directory's size. That is what
/// makes the priority hold in *every* regime rather than only in matching
/// ones, and it is why a ratio was the wrong shape for this constant.
pub const DEPTH_SPILL_MIN_FREE_HEADROOM_BYTES: u64 = SPILL_SOFT_CEILING_FREE_RESERVE_BYTES;

const _: () = assert!(
    DEPTH_SPILL_MIN_FREE_HEADROOM_BYTES > SPILL_MIN_FREE_HEADROOM_BYTES,
    "depth's per-write floor MUST exceed the tick tier's, or the two \
     defences disagree about which lane gives way first."
);

/// Pre-register both `tier` label values of `tv_spill_free_probe_blind_total`
/// at 0.
///
/// That counter records rescues written WITHOUT a free-space answer, because
/// the `df` probe failed. A per-write floor must NOT fail closed — one broken
/// probe would disable the entire rescue tier — so the write proceeds; what
/// was wrong before is that it proceeded SILENTLY.
///
/// A counter that is never incremented is never REGISTERED as a series, and an
/// absent series is indistinguishable from a healthy zero — the failure this
/// repository has already paid for once, on the depth loss counters.
///
/// Emitted as a STRING LITERAL at every site, deliberately:
/// `loss_writer_metrics_are_shipped_guard` extracts metric names by scanning
/// for literals, so routing this through a `const` would make it invisible to
/// the very guard added to catch unshipped loss counters. It is listed in that
/// guard's `DELIBERATELY_LOCAL_ONLY` with its cost reason instead, so the
/// guard stays honest about it rather than silently not noticing.
pub fn seed_spill_free_probe_blind_counters() {
    metrics::counter!("tv_spill_free_probe_blind_total", "tier" => "tick").increment(0);
    metrics::counter!("tv_spill_free_probe_blind_total", "tier" => "depth").increment(0);
}

// THE CROSS-KIND ASSERT — the one whose absence let the inversion through.
//
// The two same-kind asserts (floor-vs-floor above, reserve-vs-reserve below)
// are both satisfiable while the inversion is live, because the tick tier's
// STRONGEST guard is its ceiling reserve while the depth tier's ALWAYS-ON
// guard is its per-write floor. Those are different kinds, so no same-kind
// comparison can relate them. This one does.
const _: () = assert!(
    DEPTH_SPILL_MIN_FREE_HEADROOM_BYTES >= SPILL_SOFT_CEILING_FREE_RESERVE_BYTES,
    "depth's ALWAYS-ON per-write floor MUST be at least the tick tier's \
     ceiling reserve. Below that there is a band of free-space values in \
     which ticks are refused and depth is still written — decision-critical \
     data discarded to make room for record-only data, which is the exact \
     inversion this split exists to prevent."
);

/// Free-space reserve the DEPTH rescue tier must leave behind — deliberately
/// LARGER than the tick tier's.
///
/// # Why the two tiers cannot share one reserve
///
/// Until 2026-09-01 they did, and it was backwards. The operator's own rule
/// for this system is that ticks are decision-critical (a strategy reads
/// folded tick state from RAM and can never wait on the database) while depth
/// is record-only (nothing reads it back — verified: zero readers in the
/// indicator, strategy and risk paths). So when disk gets tight, the lane
/// that must survive is ticks.
///
/// A single shared reserve produces the opposite outcome, because depth is
/// the bigger writer by every measure:
///
/// | | ticks | depth |
/// |---|---|---|
/// | rows per packet | 1 | 10 (5 levels x 2 sides) |
/// | disk footprint (measured) | 14 GB | 110 GB |
/// | rescue success (measured 2026-09-01) | 84.6% | 23.7% |
///
/// With one reserve, depth consumes the remaining free space first and BOTH
/// tiers then refuse together — so the lane nothing reads starves the lane
/// every trade depends on. The 84.6% vs 23.7% split is that already happening.
///
/// # The derivation
///
/// Depth must stop rescuing while there is still room for an ENTIRE session
/// of ticks to spill on top of the database's own reserve:
///
/// ```text
/// DEPTH_SPILL_FREE_RESERVE_BYTES
///   = SPILL_SOFT_CEILING_FREE_RESERVE_BYTES   (the database keeps operating)
///   + WORST_CASE_SESSION_TICK_SPILL_BYTES     (every tick of a session fits)
/// ```
///
/// That is 32 GiB against the tick tier's 16 GiB. The 16 GiB BAND between
/// them is the point of the whole change: inside it depth refuses and ticks
/// are still rescued, which is the priority order stated above expressed as
/// a number rather than as a comment.
///
/// A bigger multiple was rejected: every extra byte here is depth data
/// discarded on a disk that still has room, which is the exact defect the
/// 2026-09-01 fix was made to stop. The reserve is sized to what ticks
/// provably need and no more.
pub const DEPTH_SPILL_FREE_RESERVE_BYTES: u64 =
    SPILL_SOFT_CEILING_FREE_RESERVE_BYTES + WORST_CASE_SESSION_TICK_SPILL_BYTES;

const _: () = assert!(
    DEPTH_SPILL_FREE_RESERVE_BYTES > SPILL_SOFT_CEILING_FREE_RESERVE_BYTES,
    "depth MUST reserve more than ticks. If these are ever equal the two \
     tiers refuse together and record-only depth can starve decision-critical \
     ticks out of their rescue path — the defect this constant exists to fix."
);

/// What the soft ceiling decides for one rescue write.
///
/// Extracted as a pure function for a reason the CI failure of 2026-09-01
/// made concrete: the test that pinned the old behaviour could only ever
/// exercise the REFUSE arm by having the ceiling refuse unconditionally,
/// because a unit test cannot manufacture a nearly-full filesystem. So the
/// arm that actually protects the database was the one arm no test could
/// reach, and the arm that discarded 243 million rows was the one it pinned.
///
/// With the decision separated from the I/O, every arm is reachable from a
/// table of numbers, including the two that matter most: at the ceiling with
/// room (allow) and at the ceiling without it (refuse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillCeilingVerdict {
    /// Below the rail. Nothing to decide.
    UnderCeiling,
    /// Past the rail, but the volume demonstrably has room. Allow, and count.
    OverCeilingWithRoom,
    /// Past the rail and free space is at or under the database reserve.
    /// Refuse — this is the case the rail exists for.
    OverCeilingNoRoom,
    /// Past the rail and free space is unknown. Refuse: an unreadable
    /// free-space number is exactly when unbounded growth is least
    /// affordable.
    OverCeilingProbeFailed,
}

/// Decides whether a rescue write may proceed past the soft ceiling.
///
/// `free` is `None` when the free-space probe failed. `reserve` is the room
/// left for the database — see [`SPILL_SOFT_CEILING_FREE_RESERVE_BYTES`].
///
/// The comparison is strictly greater-than, so a volume sitting exactly on
/// the reserve refuses. At a boundary the safe direction is the database's.
#[must_use]
pub const fn classify_spill_ceiling(
    dir_bytes: u64,
    ceiling: u64,
    free: Option<u64>,
    reserve: u64,
) -> SpillCeilingVerdict {
    if dir_bytes < ceiling {
        return SpillCeilingVerdict::UnderCeiling;
    }
    match free {
        Some(free_bytes) if free_bytes > reserve => SpillCeilingVerdict::OverCeilingWithRoom,
        Some(_) => SpillCeilingVerdict::OverCeilingNoRoom,
        None => SpillCeilingVerdict::OverCeilingProbeFailed,
    }
}

/// What a tier does at the per-write floor when the free-space probe FAILS.
///
/// The two tiers answer this differently ON PURPOSE, and the difference is the
/// whole point of the type existing rather than each site hard-coding an `else`
/// arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlindWritePolicy {
    /// Write anyway, counted. Correct for TICKS: one broken `df` must not
    /// disable the tier that carries decision-critical rows.
    FailOpen,
    /// Refuse. Correct for DEPTH: it is record-only, carries ~24x the tick row
    /// volume, and writing it blind is how the volume fills under ticks.
    FailClosed,
}

/// The per-write floor's verdict, separated from the I/O for the same reason
/// [`SpillCeilingVerdict`] was: an arm reachable only when `df` is broken on a
/// live box is an arm no test can otherwise drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillFloorVerdict {
    /// Free space is known and sufficient for the payload plus the floor.
    Allow,
    /// Free space is known and insufficient. Refuse.
    RefuseNoRoom,
    /// The probe failed and this tier writes blind, counted.
    AllowProbeFailed,
    /// The probe failed and this tier refuses rather than write blind.
    RefuseProbeFailed,
}

/// Decides whether a rescue write may proceed at the per-write floor.
///
/// # Why the blind arm is asymmetric, and why that asymmetry is now a TYPE
///
/// Found by an adversarial sweep on 2026-09-01, hours after the reserve split
/// it defeats was shipped. Both tiers refuse on a failed probe INSIDE their
/// ceiling arm, and that arm engages only once the tier's OWN spill directory
/// is past its rail. The two directories are INDEPENDENT. So with `df` broken,
/// the tick directory over its ceiling, and the depth directory still under
/// its cap:
///
/// | tier | path | outcome |
/// |---|---|---|
/// | tick | `OverCeilingProbeFailed` | **REFUSE** |
/// | depth | ceiling arm skipped, floor fails open | **WRITE** |
///
/// Decision-critical ticks discarded while record-only depth is written — the
/// exact inversion the two-reserve split exists to prevent, arriving through
/// the one door the split's `const _` assert cannot watch. That assert compares
/// BYTE VALUES, and the probe-failure path carries no byte value at all: it is
/// the whole `None` domain rather than a band inside it.
///
/// It is reachable, not theoretical. A single-table WAL suspension grows one
/// spill directory and not the other, which is what happened on 2026-08-25
/// when fourteen tables suspended individually.
///
/// A `floor` of zero means the floor is DISABLED (the value tests inject when
/// they need the allow arm without depending on the build machine's free
/// space). A disabled floor cannot refuse, blind or otherwise — otherwise
/// adding this policy would silently break every test that injects zero.
#[must_use]
pub const fn classify_spill_floor(
    free: Option<u64>,
    payload_len: u64,
    floor: u64,
    blind: BlindWritePolicy,
) -> SpillFloorVerdict {
    match free {
        Some(free_bytes) => {
            // saturating: a payload near u64::MAX must refuse, never wrap into
            // a small `needed` that then compares as plenty of room.
            let needed = payload_len.saturating_add(floor);
            if free_bytes < needed {
                SpillFloorVerdict::RefuseNoRoom
            } else {
                SpillFloorVerdict::Allow
            }
        }
        None => match blind {
            BlindWritePolicy::FailOpen => SpillFloorVerdict::AllowProbeFailed,
            // NO `floor == 0` CARVE-OUT (removed 2026-09-01, adversarial review).
            //
            // It read reasonably — "a disabled floor stays disabled even when
            // blind" — and it was a trap. `DepthWriter::for_test` is `pub`,
            // sets `spill_min_free_headroom: 0`, and IS reachable from
            // production: `dhan_feed_stack` builds one as a `mem::replace`
            // placeholder. Today that placeholder is overwritten two lines
            // later, so the window is transient — but any future edit that
            // returns or `?`s in between would leave a fail-CLOSED tier
            // silently writing BLIND, which is the precise inversion this
            // policy type was introduced to make impossible.
            //
            // The policy is about BLINDNESS, not about the floor value. A
            // tier that asked to fail closed when it cannot see free space
            // still cannot see free space when its floor happens to be zero.
            BlindWritePolicy::FailClosed => SpillFloorVerdict::RefuseProbeFailed,
        },
    }
}

/// Reads free bytes for `dir`, or `None` when the probe failed.
///
/// Collapses the probe's richer outcome to the one bit
/// [`classify_spill_ceiling`] needs, so the decision stays a pure function of
/// numbers rather than of a filesystem type.
#[must_use]
pub fn spill_free_bytes(dir: &std::path::Path) -> Option<u64> {
    match crate::disk_health_watcher::probe_disk_free_bytes(dir) {
        crate::disk_health_watcher::DiskHealthOutcome::Ok { free_bytes, .. } => Some(free_bytes),
        crate::disk_health_watcher::DiskHealthOutcome::ProbeFailed { .. } => None,
    }
}

/// Fraction of the volume the spill tier may occupy: one thirty-second.
///
/// Not a round number chosen for looks. It has to satisfy two bounds at once:
/// large enough that a realistic episode of flush failures is absorbed rather
/// than dropped, and small enough that the tier can never threaten the database
/// it is rescuing from. On the live 200 GB volume it yields ~6.25 GB — twelve
/// times the old ceiling, and still leaving over 96.8% of the disk for QuestDB,
/// the frame WAL and everything else.
pub const TICK_SPILL_VOLUME_FRACTION: u64 = 32;

/// Ceiling on the spill directory, in bytes — DERIVED from the volume.
///
/// Resolved ONCE into a `OnceLock`, so every subsequent read is O(1) and the
/// enforcement can never disagree with the number a log line quotes. An
/// unmeasurable volume falls back to [`TICK_SPILL_MIN_MAX_BYTES`] with a coded
/// warning, never silently.
///
/// # Complexity
/// O(1) after the first call. The first call spawns one `df`, on the cold
/// flush-failure path only.
#[must_use]
pub fn tick_spill_max_bytes() -> u64 {
    static RESOLVED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        let probe_at = std::path::Path::new(TICK_SPILL_DIR);
        // Probe the deepest EXISTING ancestor: `df` on a path that does not
        // exist yet fails, and the spill dir is created lazily.
        let mut probe: &std::path::Path = probe_at;
        while !probe.exists() {
            match probe.parent() {
                Some(parent) => probe = parent,
                None => break,
            }
        }
        match crate::disk_health_watcher::probe_disk_free_bytes(probe) {
            crate::disk_health_watcher::DiskHealthOutcome::Ok { total_bytes, .. }
                if total_bytes > 0 =>
            {
                let derived = total_bytes / TICK_SPILL_VOLUME_FRACTION;
                // Never BELOW what the fixed cap already allowed: this change
                // exists to stop losing ticks, so it must not reduce headroom
                // on a small volume.
                let ceiling = derived.max(TICK_SPILL_MIN_MAX_BYTES);
                tracing::info!(
                    total_bytes,
                    ceiling_bytes = ceiling,
                    fraction = TICK_SPILL_VOLUME_FRACTION,
                    "tick spill ceiling derived from the volume — a failed flush can be \
                     rescued to disk up to this size before rows are dropped"
                );
                ceiling
            }
            _ => {
                tracing::warn!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    fallback_bytes = TICK_SPILL_MIN_MAX_BYTES,
                    "could not measure the spill volume, so the tick spill ceiling falls back \
                     to the fixed floor. Rescues past that size will be refused and their \
                     ticks dropped — on a large volume this is far more conservative than \
                     necessary"
                );
                TICK_SPILL_MIN_MAX_BYTES
            }
        }
    })
}

/// Appends a failed flush's ILP payload to the spill directory.
///
/// # Why the payload is stored verbatim
///
/// `Buffer::as_bytes()` is InfluxDB line protocol — exactly the body
/// QuestDB's own `/write` endpoint accepts. So a spill file is not an archive
/// format needing a bespoke parser; it is directly re-ingestable:
///
/// ```text
/// curl --data-binary @data/spill/ticks/<file> http://localhost:9000/write
/// ```
///
/// Re-ingest is idempotent because the `ticks` DEDUP key
/// `(ts, security_id, segment, capture_seq, feed)` carries `capture_seq`, so a
/// replayed row UPSERTs onto itself rather than duplicating.
///
/// # Errors
///
/// `Err` when the directory cannot be created or the append fails. The caller
/// treats that as "rescue unavailable" and falls back to the counted drop —
/// a spill that cannot be written must never mask the loss.
fn spill_failed_ilp(
    dir: &Path,
    payload: &[u8],
    feed: Feed,
    now_unix_secs: i64,
) -> std::io::Result<PathBuf> {
    // O(1) EXEMPT: begin — cold path, runs only on a flush failure.
    std::fs::create_dir_all(dir)?;

    // The ceiling is a SOFT rail, not a refusal.
    //
    // MEASURED IN PRODUCTION 2026-09-01: this arm refused four times with
    // `ceiling = 10_063_871_360` (1/32 of a 322 GB volume) while `df` reported
    // **143 GB free**. 5,142,980 ticks were permanently discarded onto a disk
    // that was 53% used. The depth twin fired 48 times the same morning and
    // discarded 238,615,500 rows. Neither disk was anywhere near full.
    //
    // The rail's INTENT is right and is kept: the spill tier must never be able
    // to starve the database it rescues from. What was wrong is the quantity it
    // measured. `tick_spill_max_bytes()` derives from the volume's TOTAL size —
    // a constant that never changes — so it fires at the same 3% whether the
    // disk is empty or nearly full. Total size cannot threaten QuestDB. Only
    // FREE space can.
    //
    // New rule: past the soft ceiling, refuse ONLY when free space is already
    // at or below the headroom the live guard below enforces anyway. Above it
    // there is demonstrably room, and discarding market data we have somewhere
    // to put is indefensible.
    let ceiling = tick_spill_max_bytes();
    let held = spill_dir_bytes(dir);
    // The probe is deliberately INSIDE this branch, not an argument to the
    // call below.
    //
    // REGRESSION FIXED 2026-09-01 (adversarial review). Extracting the
    // decision into a pure function made `spill_free_bytes(dir)` an eagerly
    // evaluated argument, so every rescue forked `df` -- including the common
    // under-ceiling case, which is most of them, and which needs no free-space
    // answer at all. Combined with the live headroom guard further down that
    // was TWO process forks per rescue where the pre-refactor code did one.
    //
    // Worth stating because it is the characteristic cost of pure functions:
    // moving a decision out of an `if` turns a lazily-computed input into an
    // unconditionally-computed one, and the compiler will not tell you.
    if held >= ceiling {
        match classify_spill_ceiling(
            held,
            ceiling,
            spill_free_bytes(dir),
            SPILL_SOFT_CEILING_FREE_RESERVE_BYTES,
        ) {
            SpillCeilingVerdict::UnderCeiling => {}
            SpillCeilingVerdict::OverCeilingWithRoom => {
                // Room to spare. Allow the rescue and record that we are past the
                // soft rail, so the growth is never silent.
                metrics::counter!("tv_tick_spill_over_soft_ceiling_total").increment(1);
            }
            SpillCeilingVerdict::OverCeilingNoRoom => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    format!(
                        "tick spill dir past its {ceiling}-byte soft ceiling and free space is \
                     at or below the {SPILL_SOFT_CEILING_FREE_RESERVE_BYTES}-byte database \
                     reserve — refusing so QuestDB keeps room to operate"
                    ),
                ));
            }
            SpillCeilingVerdict::OverCeilingProbeFailed => {
                // COUNT THE BLINDNESS (2026-09-01, adversarial review).
                //
                // Until now only the FLOOR arms touched this counter, so a
                // broken `df` refusing every rescue AT THE CEILING left the
                // tier permanently refusing with `tv_spill_free_probe_blind_total`
                // sitting at its seeded 0. The operator would watch
                // `tv_ticks_dropped_total` climb with nothing naming the
                // cause — and a broken probe is the one cause with a
                // one-command fix. Same counter, same label, so a blind
                // episode reads as one number wherever it bites.
                metrics::counter!("tv_spill_free_probe_blind_total", "tier" => "tick").increment(1);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    format!(
                        "tick spill dir past its {ceiling}-byte soft ceiling and the free-space \
                     probe failed — refusing rather than growing blind"
                    ),
                ));
            }
        }
    }
    // LIVE FREE-SPACE GUARD (2026-08-25). The ceiling above is derived from
    // the volume's TOTAL size, which is the right bound for "how much of this
    // disk may the spill tier own" — and the wrong question to ask a disk that
    // is nearly full RIGHT NOW.
    //
    // Measured on the prod box the day this landed: free space cycling between
    // 1.94 GB and 12.9 GB on a 200 GB volume, because QuestDB stages large
    // files and releases them. A 544 MB rescue — the real observed size —
    // landing in a 1.94 GB trough would take the disk to under 1.4 GB and can
    // take QuestDB down with it, which is the outage this whole tier exists to
    // avoid trading a bounded tick loss for.
    //
    // So the write is refused when it would leave less than
    // `SPILL_MIN_FREE_HEADROOM_BYTES` behind. This is deliberately NOT
    // memoised, unlike the ceiling: free space is a moving quantity, and a
    // cached value would be answering a question about a disk that no longer
    // exists. One `df` per rescue is free — a rescue only happens when a flush
    // already failed.
    //
    // A refusal here is still an honest counted drop, not a silent one: the
    // caller's `Err` arm logs HOT-PATH-02 naming this reason.
    // ASYMMETRY WITH THE CEILING ARM, DELIBERATE — flagged by adversarial
    // review 2026-09-01 and kept, with the fall-through made VISIBLE.
    //
    // The ceiling arm fails CLOSED on a probe failure; this floor falls
    // through and writes. Same file, opposite semantics — which reads like an
    // oversight and is not. The two arms ask different questions:
    //
    //   ceiling — "we are ALREADY past our size rail; may we keep growing?"
    //             Unknown => do not grow. It bites only in an already
    //             abnormal state, so refusing costs little.
    //   floor   — "is there room for THIS one write?" Unknown => refusing
    //             means a single broken `df` disables the entire rescue tier
    //             for the process lifetime, turning every failed flush back
    //             into the permanent loss this tier exists to prevent.
    //
    // So the fail-open is the right call. What was actually wrong is that it
    // was SILENT: the tier could write blind, indefinitely, with nothing
    // saying so. It is now counted.
    // FAIL OPEN when the probe fails, and that is deliberate for THIS tier:
    // one broken `df` must not disable the rescue path that carries
    // decision-critical rows. The depth twin fails CLOSED for the mirror-image
    // reason — see `classify_spill_floor` for why the asymmetry is now a typed
    // policy rather than two hand-written `else` arms that silently diverged.
    let free_now = spill_free_bytes(dir);
    match classify_spill_floor(
        free_now,
        payload.len() as u64,
        SPILL_MIN_FREE_HEADROOM_BYTES,
        BlindWritePolicy::FailOpen,
    ) {
        SpillFloorVerdict::Allow => {}
        SpillFloorVerdict::AllowProbeFailed => {
            metrics::counter!("tv_spill_free_probe_blind_total", "tier" => "tick").increment(1);
        }
        SpillFloorVerdict::RefuseNoRoom | SpillFloorVerdict::RefuseProbeFailed => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "refusing a {}-byte tick spill: {} free, and the write must leave \
                     {SPILL_MIN_FREE_HEADROOM_BYTES} bytes of headroom so it cannot take \
                     QuestDB down with it",
                    payload.len(),
                    match free_now {
                        Some(b) => format!("only {b} bytes"),
                        None => "an unreadable amount".to_string(),
                    }
                ),
            ));
        }
    }

    // One file per feed per hour: bounded file count, and an operator replaying
    // a known-bad window does not have to read a single ever-growing file.
    let hour = now_unix_secs / 3_600;
    let path = dir.join(format!("ticks-{}-{hour}.ilp", feed.as_str()));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(payload)?;
    file.flush()?;
    Ok(path)
    // O(1) EXEMPT: end
}

/// Total bytes of the spill directory, INCLUDING the quarantine subtree.
///
/// # Why quarantine is counted (2026-08-28)
///
/// This was non-recursive, and `quarantine/` is a subdirectory — so files the
/// replay set aside as permanently refused counted toward NOTHING: not this
/// ceiling, and not any free-space check. They accumulated with no bound at
/// all, on the same volume that filled completely on 2026-08-25 and
/// WAL-suspended fifteen tables while every writer reported success.
///
/// Quarantined files are not junk. The quarantine docstring records that of
/// 1,293 rows in one refused file, 1,292 were recoverable — a whole file is
/// set aside because ONE row is malformed. So they are real ticks, they are
/// never retried, and they were silently eating the disk.
///
/// Counting them is a deliberate trade and worth stating both ways. It means a
/// large quarantine can consume the rescue ceiling and cause new spills to be
/// refused — bounded, counted, loud tick loss. Not counting them means the
/// quarantine consumes the VOLUME instead, which is unbounded and takes every
/// table down with it. The 2026-08-25 incident is what that second option
/// actually costs, so the first is the better failure.
///
/// Still best-effort: an unreadable entry contributes 0 rather than aborting
/// the sweep, because failing to MEASURE the cap must not also fail the rescue
/// the cap is protecting. Descent is exactly one level deep — the quarantine
/// subtree is flat — so this cannot become an unbounded walk.
fn spill_dir_bytes(dir: &Path) -> u64 {
    // O(1) EXEMPT: begin — cold path, bounded by the per-feed-per-hour file count.
    fn files_in(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .filter_map(std::result::Result::ok)
            .filter_map(|e| e.metadata().ok())
            .filter(std::fs::Metadata::is_file)
            .map(|m| m.len())
            .sum()
    }
    files_in(dir).saturating_add(files_in(
        &dir.join(crate::tick_spill_replay::QUARANTINE_DIR),
    ))
    // O(1) EXEMPT: end
}

/// The share of the tick spill ceiling that quarantined files may occupy.
///
/// # Why quarantine needs a bound at all
///
/// `quarantine_spill_file` promises QUARANTINE, NEVER DELETE, and that promise
/// is right: on 2026-08-25 a single torn line stranded a file whose other 1,292
/// lines were recoverable by hand, and a rescue tier that destroys what it
/// cannot parse is worse than the loss it prevents.
///
/// But nothing in this workspace ever removed from `quarantine/`, and since
/// 2026-08-28 its bytes count toward the spill ceiling. Those two facts
/// together are a RATCHET: once accumulated quarantine reaches
/// `tick_spill_max_bytes()`, `spill_failed_ilp` returns `StorageFull` for every
/// future rescue — for this process AND every subsequent boot — and the only
/// symptom is a HOT-PATH-02 drop that reads like ordinary backpressure. The
/// rescue tier would be permanently dead, killed by the tier that exists to
/// preserve data.
///
/// So the promise is kept for a BOUNDED share rather than forever. A quarter,
/// because the arithmetic has to leave the LIVE rescue path the clear majority
/// of the budget: quarantine holds files QuestDB has already refused, while the
/// remaining three quarters hold rows it would still accept. Preferring the
/// refused ones would trade recoverable-by-hand bytes for rows lost outright.
pub const QUARANTINE_BUDGET_FRACTION: u64 = 4;

/// Counter: quarantined files deleted to keep the quarantine directory inside
/// its share of the spill ceiling.
pub const QUARANTINE_PRUNED_COUNTER: &str = "tv_tick_spill_quarantine_pruned_total";

/// Trims the quarantine directory to its byte budget, OLDEST FIRST.
///
/// Boot-time only, and loud: every deletion names the file and the byte count,
/// because this is the one place in the rescue chain that destroys data on
/// purpose. Returns the number of files removed.
///
/// Oldest-first because a quarantined file's recoverability decays: the operator
/// who could reconstruct today's torn line from the day's context cannot do that
/// for a file from three weeks ago, and the newest files are the ones an active
/// investigation is about.
///
/// A directory that cannot be read is not an error here — there may simply be no
/// quarantine yet, and failing a boot over a missing subdirectory would turn a
/// housekeeping step into an outage.
pub fn prune_quarantine(spill_dir: &Path, spill_max_bytes: u64) -> usize {
    let budget = spill_max_bytes / QUARANTINE_BUDGET_FRACTION;
    let dir = spill_dir.join(crate::tick_spill_replay::QUARANTINE_DIR);
    // O(1) EXEMPT: begin — boot-time housekeeping over a flat directory, never
    // on any per-tick or per-frame path.
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut files: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((
                meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                meta.len(),
                e.path(),
            ))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total <= budget {
        return 0;
    }
    files.sort_by_key(|(mtime, _, _)| *mtime);
    let mut removed = 0usize;
    for (_, len, path) in files {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
            removed += 1;
            metrics::counter!(QUARANTINE_PRUNED_COUNTER).increment(1);
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                path = %path.display(),
                bytes = len,
                budget,
                "DELETED a quarantined tick spill file to keep the quarantine directory \
                 inside its share of the spill ceiling. Its recoverable lines are gone. \
                 This is deliberate and it is the lesser loss: quarantine counts toward \
                 the spill ceiling, so an unbounded quarantine would return StorageFull \
                 for every future rescue — permanently disabling the tier that keeps \
                 live ticks, for this process and every boot after it."
            );
        }
    }
    // O(1) EXEMPT: end
    removed
}
/// ILP-over-HTTP conf: per-flush server ACK (the 2026-07-05 fire-and-forget
/// lesson) with `retry_timeout=0` (the caller owns retry cadence) and a bounded
/// `request_timeout` so a hung flush cannot wedge the pipeline.
fn ticks_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// The designated timestamp for a `ticks` row, in IST-epoch nanoseconds.
///
/// # The problem this solves
///
/// Dhan signals "this contract has not traded today" with an LTT that is not a
/// time at all — measured on the live box 2026-08-21, it is **315,532,800**
/// (1980-01-01) for 945,501 rows and a literal `0` for 7,282 more. Those rows
/// are NOT empty: 99.2% of them carry a real `total_buy_qty`, `total_sell_qty`
/// and previous `close`. They are contracts with a live order book that simply
/// have no last trade yet, and the drain keeps them deliberately — discarding
/// them would lose the ability to tell "did not trade" from "did not capture",
/// and would throw away the book with it.
///
/// Stamping the row with the vendor's sentinel put ~950,000 rows of real data
/// per session into a permanent `1980-01-01` partition, where **no time-range
/// query can ever reach them**. Data that cannot be found is not kept in any
/// useful sense, and every one of those writes also lands out-of-order against
/// the current partition rather than appending to it.
///
/// # The rule
///
/// An LTT below [`MIN_PLAUSIBLE_EXCHANGE_TS_SECS`] is a sentinel, not a time,
/// so the row is stamped with its RECEIPT time — which is the only real time
/// such an observation has. The raw sentinel is NOT destroyed: it stays in the
/// `exchange_timestamp` column, so "never traded" remains recoverable, and the
/// same floor the aggregator uses to refuse the candle is the one used here, so
/// the two cannot drift apart.
///
/// # Convention safety
///
/// Both inputs are already IST-epoch (`.claude/rules/project/data-integrity.md`
/// calls the WebSocket timestamp rule "THE SINGLE MOST CRITICAL DATA INTEGRITY
/// RULE"): `exchange_timestamp` arrives IST-epoch with NO `+19800` applied, and
/// `received_at_ist_nanos` has already had the offset added by its caller. So
/// this function only ever CHOOSES between two values in one space — it never
/// converts, and cannot introduce the sign error that rule warns about.
///
/// # Fail-safe
///
/// If the LTT is a sentinel AND no receipt time was carried, there is no real
/// time available, so the sentinel is kept unchanged. That row stays out of the
/// live range exactly as it does today — a guess would be worse than a value
/// that is visibly wrong.
/// Counts and reports a non-finite OPTIONAL price, throttled to powers of two.
///
/// The counter and the log line live together deliberately. `crates/common`'s
/// `loss_counter_visibility_guard` refuses a loss counter that reaches no
/// operator surface — "a counter that measures data loss and reaches nobody is
/// worse than no counter: the loss is measured, the measurement is discarded,
/// and the dashboard stays green" — and it is right. The counter alone was the
/// first version of this fix and the guard caught it.
///
/// The counter is now EMF-SHIPPED as well as logged.
///
/// ⚠ CORRECTED 2026-09-01. This paragraph used to read: *"A `warn!` rather
/// than an EMF metric because the deployed selector list lives in a user-data
/// template that currently renders at EXACTLY its 15,872-byte budget, with
/// zero free bytes."*
///
/// **Both halves of that were false by the time they were being relied on.**
/// The selector no longer lives in the user-data template at all — it moved to
/// `deploy/aws/cloudwatch-agent.json` on 2026-08-25 and a guard now *forbids*
/// a second copy in the template. And the template does not render at its
/// budget: measured 2026-09-01 by running the guard, it renders **13,869 of
/// 15,872 bytes, with 2,003 free**.
///
/// So a drop counter on the one writer that can permanently destroy market
/// data was kept off CloudWatch by a blocker that had already been removed.
/// The reusable lesson is not "check the number" — it is that a MEASUREMENT
/// copied into a justification carries no date, and this one was copied from a
/// rule-file section that is itself stale. Adding an EMF name now costs
/// ~$0.30/mo and **zero** user-data bytes.
///
/// Powers of two: 1, 2, 4, 8, … The first occurrence always logs, the rate
/// decays logarithmically so a corruption storm cannot flood the sink, and the
/// running total rides in the line so a throttled message still states the true
/// magnitude rather than implying a single event.
fn note_non_finite_optional_price(security_id: u64, segment: &'static str) {
    static SEEN: AtomicU64 = AtomicU64::new(0);
    metrics::counter!("tv_tick_optional_price_dropped_total").increment(1);
    let total = SEEN.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    if total.is_power_of_two() {
        warn!(
            code = ErrorCode::StorageGapTickDedupSegment.code_str(),
            security_id,
            segment,
            dropped_total = total,
            "an OPTIONAL tick price was non-finite and is stored as NULL rather \
             than sent to the database, where a NaN would make QuestDB reject the \
             whole batch. The tick itself is kept — its mandatory prices are \
             valid. This log is throttled to powers of two; dropped_total is the \
             true running count."
        );
    }
}

#[must_use]
pub fn row_timestamp_ist_nanos(exchange_timestamp: u32, received_at_ist_nanos: Option<i64>) -> i64 {
    let ltt_nanos = i64::from(exchange_timestamp).saturating_mul(1_000_000_000);
    // A CEILING as well as a floor, added 2026-08-25.
    //
    // This had only the floor, and `exchange_timestamp` is a `u32` read raw
    // off the wire: `0xFFFFFFFF` is ~year 2106 and sailed past a
    // `>= MIN_PLAUSIBLE` test. `ts` is the DESIGNATED timestamp, so such a row
    // creates a far-future QuestDB partition that retention and archival — both
    // keyed on the trading day — can never reach, while every `max(ts)` and
    // range scan over `ticks` silently includes it.
    //
    // The aggregator now refuses that tick outright (the band check moved above
    // its untraded-sentinel return in the same change), so this ceiling is
    // defence in depth rather than the only guard. It is here anyway because a
    // second writer must not be able to reintroduce the hole by calling this
    // helper directly: the band belongs where the stamp is MADE.
    //
    // Out of band falls back to the receipt time, exactly as below-floor does.
    use tickvault_trading::candles::multi_tf_aggregator::{
        MAX_PLAUSIBLE_EXCHANGE_TS_SECS, MIN_PLAUSIBLE_EXCHANGE_TS_SECS,
    };
    if (MIN_PLAUSIBLE_EXCHANGE_TS_SECS..=MAX_PLAUSIBLE_EXCHANGE_TS_SECS)
        .contains(&exchange_timestamp)
    {
        return ltt_nanos;
    }
    received_at_ist_nanos.unwrap_or(ltt_nanos)
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
    /// Lowest / highest `capture_seq` among the pending rows (`0` = none).
    /// Carried on every hand-off and rescue so the WAL applied-watermark can
    /// learn which captured frames this batch covers once it LANDS.
    pending_min_seq: u64,
    pending_max_seq: u64,
    /// The `feed` SYMBOL stamped on every row from this writer.
    feed: Feed,
    /// Highest `capture_seq` this connection has stamped — the per-connection
    /// monotonicity witness (`0` before the first row).
    last_capture_seq: i64,
    /// Where a failed flush rescues its ILP payload.
    ///
    /// A field rather than a constant so the rescue PATH itself is testable:
    /// a test that can only exercise the helper proves the file format and
    /// not that `discard_pending` actually calls it.
    spill_dir: PathBuf,
    /// Set by [`TickWriter::split_for_offload`]. When present, `flush` hands
    /// the buffer to the writer thread instead of doing the network round
    /// trip inline.
    ///
    /// `None` by DEFAULT and on every existing constructor, so a writer that
    /// was never split behaves byte-for-byte as before — this is an opt-in
    /// added to one call site, not a behaviour change to the type.
    offload: Option<std::sync::mpsc::SyncSender<FlushBatch>>,
    /// Set by [`TickWriter::split_rescue_offload`]. When present,
    /// [`TickPersistenceWriter::discard_pending`] hands the buffer to a
    /// dedicated rescue thread instead of writing up to 32 MiB to disk on the
    /// frame-drain task.
    ///
    /// `None` by DEFAULT and on every existing constructor, so an unsplit
    /// writer behaves exactly as before. Separate from `offload` on purpose:
    /// the rescue exists precisely for the two cases where the offload queue
    /// cannot help — it is FULL, or its thread is GONE.
    rescue: Option<std::sync::mpsc::SyncSender<RescueBatch>>,
    /// Consecutive flushes whose rows the producer RETAINED because the queue
    /// was full. Reset to zero on every successful hand-off.
    ///
    /// This is the batch-WIDTH bound, and it is the one the live measurement
    /// made load-bearing — see [`MAX_RETAINED_FLUSH_SPANS`].
    retained_spans: u32,
}

/// Publish a zero on this feed's drop series before any row can be written.
///
/// The CloudWatch agent computes a counter's alarm value as the DELTA between
/// consecutive samples and drops the first sample of a series it has never
/// seen. `tv_ticks_dropped_total` increments ONLY when buffered rows are
/// discarded on a flush failure, so without this the first drop episode IS the
/// dropped baseline sample: it publishes no datapoint and
/// `tv-<env>-ticks-dropped` (threshold 1, one 300s period) does not fire. A
/// single backpressure episode — the ordinary shape — would be silently
/// unwatched, which is the exact false-OK that alarm exists to prevent.
///
/// Registered per FEED, matching the emit site's label, because the agent
/// baselines per Prometheus series and the EMF processor folds the labels to
/// `{host}` afterwards by summing per-series deltas: an unregistered feed
/// contributes nothing on the sample where it is born. Same discipline as
/// `WalRingSink::pre_register`.
///
/// Called from EVERY constructor rather than just the production one — see
/// [`TickWriter::for_test`] for why that is not merely tidiness. Idempotent:
/// `increment(0)` on an already-registered series is a no-op.
fn register_drop_baseline(feed: Feed) {
    metrics::counter!("tv_ticks_dropped_total", "feed" => feed.as_str()).increment(0);
    // The rescue counter needs the SAME treatment for the same reason: a
    // spill episode is rare, so its first increment would otherwise be
    // consumed as the delta baseline and the episode would go unreported.
    metrics::counter!("tv_ticks_spilled_total", "feed" => feed.as_str()).increment(0);
    // Same discipline for the persist-error series (2026-08-28). These fire
    // only on a broken boot, which makes them RARER than a drop episode, not
    // safer: a first occurrence with no prior sample is consumed as the delta
    // baseline and publishes nothing, so the alarm is silent through the one
    // event it exists for. Both `stage` label values are seeded, because a
    // label set is a separate series.
    metrics::counter!("tv_tick_persist_errors_total", "stage" => "ensure_client_build")
        .increment(0);
    metrics::counter!("tv_tick_persist_errors_total", "stage" => "ensure_ddl").increment(0);
    // The rescue-ABANDONED series needs the same seed, and it is the one that
    // matters most: it fires exactly when `dropped == spilled` stops being
    // true — rows counted as rescued whose payload may never have reached the
    // spill file. Unseeded, its first and only increment is consumed as the
    // delta baseline, so the one event that invalidates the rescue proof
    // publishes nothing.
    metrics::counter!(TICK_RESCUE_ABANDONED_COUNTER, "writer" => "tick").increment(0);
}

impl TickWriter {
    /// Production constructor — ILP-over-HTTP, lazy on connect failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor; the lazy-build contract is
    // exercised by tick_writer_new_is_lazy_and_buffers_without_network, and every
    // append/flush path is covered via for_test().
    pub fn new(config: &QuestDbConfig, feed: Feed) -> Self {
        register_drop_baseline(feed);
        match Sender::from_conf(ticks_ilp_http_conf(config)) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                    pending_min_seq: 0,
                    pending_max_seq: 0,
                    feed,
                    last_capture_seq: 0,
                    spill_dir: PathBuf::from(TICK_SPILL_DIR),
                    offload: None,
                    rescue: None,
                    retained_spans: 0,
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
                    pending_min_seq: 0,
                    pending_max_seq: 0,
                    feed,
                    last_capture_seq: 0,
                    spill_dir: PathBuf::from(TICK_SPILL_DIR),
                    offload: None,
                    rescue: None,
                    retained_spans: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer with an empty buffer.
    ///
    /// Registers the same baseline as [`TickWriter::new`], deliberately. This
    /// is `pub` and NOT `#[cfg(test)]`-gated — it cannot be, because
    /// `crates/app`'s own tests construct it across the crate boundary, where
    /// `cfg(test)` does not reach. That makes it a real bypass of the drop
    /// baseline if the registration lives only in `new`, so the registration
    /// lives in one shared place that both constructors call.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    /// Redirects the rescue tier at a scratch dir.
    ///
    /// Test-only: production always uses [`TICK_SPILL_DIR`].
    #[cfg(test)]
    pub fn with_spill_dir_for_test(mut self, dir: PathBuf) -> Self {
        self.spill_dir = dir;
        self
    }

    pub fn for_test(feed: Feed) -> Self {
        register_drop_baseline(feed);
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
            pending_min_seq: 0,
            pending_max_seq: 0,
            feed,
            last_capture_seq: 0,
            spill_dir: PathBuf::from(TICK_SPILL_DIR),
            offload: None,
            rescue: None,
            retained_spans: 0,
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
        let outcome = match TickRow::from_parsed_tick(tick, capture_seq) {
            Ok(row) => self.append_row(&row),
            Err(err) => Err(err.into()),
        };
        if let Err(err) = &outcome {
            // The frame was captured to the WAL and will NOT reach the
            // database from this session: the next replay must re-offer it.
            crate::wal_applied_watermark::applied_watermark()
                .note_unapplied(u64::try_from(capture_seq).unwrap_or(0));
            // A LOSS, counted on the ALARMED metric — found 2026-09-01.
            //
            // Until this line the only record of an append failure was the
            // drain's `tv_dhan_feed_drain_frames_total{outcome="write_failed"}`
            // label. The EMF processor folds every label value of one metric
            // into a single summed series per host, so that failure was added
            // to the ~83M `folded` frames of a session and vanished: no alarm
            // reads the label, no dashboard line separates it, and the coded
            // ERROR line carried no `source` a log filter could key on. A
            // poisoned buffer could have refused every tick of a session with
            // every loss pager green.
            //
            // `tv_ticks_dropped_total` is the right home, not a new name: it
            // means "rows left the fold without reaching QuestDB", which is
            // exactly this, and `dhan_ticks_dropped` (`live-lane-alarms.tf`)
            // pages on `dropped - spilled >= 1`. An append failure never
            // spills — there is no row to rescue — so it lands on the
            // permanent-loss side of that subtraction, which is where it
            // belongs. Same `feed` label as the seeded baseline so the series
            // exists from boot rather than registering on the first failure.
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                feed = self.feed.as_str(),
                security_id = tick.security_id,
                capture_seq,
                source = "ilp_append_failed",
                error = %err,
                "tick row could not be appended to the ILP buffer — LOST, counted on \
                 tv_ticks_dropped_total (the row never existed, so nothing spills)"
            );
            metrics::counter!("tv_ticks_dropped_total", "feed" => self.feed.as_str()).increment(1);
        }
        outcome
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
        self.note_pending_seq(row.capture_seq);
        Ok(())
    }

    /// Widens the pending sequence range to cover one appended row.
    fn note_pending_seq(&mut self, capture_seq: i64) {
        let seq = u64::try_from(capture_seq).unwrap_or(0);
        if seq == 0 {
            return;
        }
        if self.pending_min_seq == 0 || seq < self.pending_min_seq {
            self.pending_min_seq = seq;
        }
        if seq > self.pending_max_seq {
            self.pending_max_seq = seq;
        }
    }

    /// Takes the pending range, leaving it empty.
    fn take_pending_range(&mut self) -> (u64, u64) {
        let r = (self.pending_min_seq, self.pending_max_seq);
        self.pending_min_seq = 0;
        self.pending_max_seq = 0;
        r
    }

    /// Restores a range the hand-off gave back (queue full / thread gone).
    fn restore_pending_range(&mut self, range: (u64, u64)) {
        self.pending_min_seq = range.0;
        self.pending_max_seq = range.1;
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
        // The offload branch sits FIRST, above the no-sender check, because a
        // split writer has no `Sender` at all — the sink took it. Checking
        // `sender.is_none()` first would treat every offloaded flush as
        // "QuestDB unreachable" and rescue every batch to disk.
        if self.offload.is_some() {
            return match self.offload_flush() {
                // Handed off. The rows are the sink's problem now, and the
                // sink is the one that reports whether they landed.
                OffloadOutcome::Sent(_) => Ok(()),
                // NOT an error. The rows are still here and still pending;
                // the next flush tries again. Returning `Err` would make the
                // drain report a loss that did not happen.
                OffloadOutcome::QueueFull(_) => Ok(()),
                OffloadOutcome::WidthCapped(rows) => {
                    anyhow::bail!(
                        "ticks: the writer stayed behind for more than \
                         {MAX_RETAINED_FLUSH_SPANS} flush span(s) — {rows} row(s) were \
                         RESCUED to the spill tier rather than widening the commit, \
                         because commit width is the measured write amplifier"
                    )
                }
                OffloadOutcome::SinkGone(rows) => {
                    anyhow::bail!(
                        "ticks: the offload writer thread is gone — {rows} row(s) rescued"
                    )
                }
            };
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
                // The synchronous arm (offload not wired, or its spawn failed)
                // acks the watermark itself; otherwise a lane on this path
                // would never skip a replayed frame.
                let (_, max_seq) = self.take_pending_range();
                crate::wal_applied_watermark::applied_watermark().note_ticks_acked(max_seq);
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

    /// Rescues every buffered-but-unflushed row to the spill tier, then clears.
    ///
    /// Replaces a bare discard. The rows are good — today's live evidence is
    /// `Could not flush buffer: http://localhost:9000/write: timeout: per
    /// call`, a CLIENT-side timeout with no matching error in QuestDB's own
    /// log, so nothing was rejected and the buffer was never poisoned. The
    /// "poisoned-buffer defence" rationale is real but applies to a row the
    /// SERVER refused; it was being applied to every failure alike, and 1,377
    /// unrepeatable ticks were dropped on 2026-08-21 because of it.
    ///
    /// Ticks are the one payload class that cannot be re-fetched — the REST
    /// legs' rows carry "rows are re-fetchable" in their own discard message
    /// and are genuinely fine to drop. So this writer, and not those, gets the
    /// rescue tier that `seal_absorption` has always had for seals.
    ///
    /// Returns the number of rows that left the buffer, whether rescued or
    /// dropped, so the caller's accounting is unchanged.
    /// Splits this writer into a PRODUCER half and a network SINK half.
    ///
    /// The producer keeps the ILP buffer and the row accounting and stays on
    /// the drain task; the sink takes the `Sender` and belongs on a thread of
    /// its own. They are joined by a bounded queue, so the drain can never be
    /// blocked by the network and can never grow the queue without bound.
    ///
    /// # Why this exists
    ///
    /// `flush` is a blocking ILP-over-HTTP round trip with a 5 s timeout, and
    /// it was being called from the frame-drain task. `block_in_place` bounded
    /// the DAMAGE — the runtime spins up a replacement worker so the other
    /// tasks keep running — but it does not remove the drain from the flush's
    /// critical path: the drain itself is what stops, and the drain is the
    /// only thing emptying the socket. A slow database therefore stalled the
    /// fold, filled the receive buffer, and Dhan — which skips a slow consumer
    /// forward to "the latest available state" with no sequence number —
    /// discarded the intermediate ticks at THEIR side, invisibly. That is the
    /// mechanism by which a storage hiccup became unrecoverable tick loss, and
    /// no amount of disk throughput removes it, because the coupling is
    /// structural rather than a matter of speed.
    ///
    /// Consuming `self` and returning a new one is deliberate: it makes the
    /// split a one-way door at the type level, so no caller can hold a handle
    /// that still believes it owns the network.
    #[must_use]
    // TEST-EXEMPT: the split itself is exercised by every offload test below,
    // each of which calls it to obtain the producer/sink pair.
    pub fn split_for_offload(
        mut self,
    ) -> (Self, TickWriterSink, std::sync::mpsc::Receiver<FlushBatch>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(FLUSH_QUEUE_DEPTH);
        let sink = TickWriterSink {
            sender: self.sender.take(),
            feed: self.feed,
            spill_dir: self.spill_dir.clone(),
        };
        self.offload = Some(tx);
        (self, sink, rx)
    }

    /// Closes the hand-off queue, so the writer thread sees the end of the
    /// stream and can exit.
    ///
    /// Shutdown-only. Dropping the sender is what turns the writer's blocking
    /// `recv` into a clean exit; without it, a caller that joins the thread
    /// waits forever on a queue nothing will ever close.
    ///
    /// Leaves the writer in the UNSPLIT state, so a flush after this point
    /// takes the synchronous arm and — with the sender long gone to the sink —
    /// rescues to the spill tier rather than silently discarding. That is the
    /// correct end-of-session behaviour: rows are on disk and named, not lost.
    pub fn close_offload(&mut self) {
        self.offload = None;
    }

    /// Hands the rescue write to a dedicated thread.
    ///
    /// Separate from [`TickWriter::split_for_offload`] because it solves the
    /// case that split CANNOT: the rescue fires exactly when the flush queue is
    /// full or its thread is gone, so it cannot ride the same queue.
    ///
    /// Returns the thread's half and the receiver. Until the caller wires
    /// them, `discard_pending` keeps writing inline — this is opt-in, and the
    /// unwired behaviour is the old behaviour.
    pub fn split_rescue_offload(
        &mut self,
    ) -> (TickRescueSink, std::sync::mpsc::Receiver<RescueBatch>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(RESCUE_QUEUE_DEPTH);
        let sink = TickRescueSink {
            spill_dir: self.spill_dir.clone(),
            feed: self.feed,
        };
        self.rescue = Some(tx);
        (sink, rx)
    }

    /// Closes the rescue queue so its thread can exit.
    ///
    /// Shutdown-only, and it leaves the writer in the INLINE state on purpose:
    /// a rescue after this point writes synchronously rather than being
    /// refused, so the end-of-session rows still reach the spill tier.
    pub fn close_rescue_offload(&mut self) {
        self.rescue = None;
    }

    /// Hands the pending buffer to the writer thread without touching the
    /// network.
    ///
    /// Uses `try_send`, never `send`: a blocking send would re-create the
    /// exact coupling the split exists to remove, just one queue further out.
    fn offload_flush(&mut self) -> OffloadOutcome {
        let rows = self.pending;
        // Read the protocol version BEFORE the replace: a fresh buffer must
        // speak the same protocol the sender negotiated, and borrowing rules
        // will not let both happen in one expression.
        let protocol = self.buffer.protocol_version();
        let (min_seq, max_seq) = self.take_pending_range();
        let batch = FlushBatch {
            buffer: std::mem::replace(&mut self.buffer, Buffer::new(protocol)),
            rows,
            min_seq,
            max_seq,
        };
        let Some(tx) = self.offload.as_ref() else {
            // Unreachable — `flush` checks. Treated as the gone arm rather
            // than silently succeeding, because "we sent it" when nothing was
            // sent is the one report that must never be wrong.
            self.buffer = batch.buffer;
            self.restore_pending_range((batch.min_seq, batch.max_seq));
            return OffloadOutcome::SinkGone(rows);
        };
        match tx.try_send(batch) {
            Ok(()) => {
                self.pending = 0;
                self.retained_spans = 0;
                crate::wal_applied_watermark::applied_watermark().note_ticks_handed_off();
                metrics::counter!(
                    "tv_tick_flush_offloaded_total",
                    "feed" => self.feed.as_str()
                )
                .increment(1);
                OffloadOutcome::Sent(rows)
            }
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                self.restore_pending_range((returned.min_seq, returned.max_seq));
                // Backpressure, not loss. Put the rows BACK and keep
                // appending — the next flush retries. This is the arm that
                // makes the bounded queue safe: without it a full queue would
                // either block the drain (the original defect) or drop rows
                // (a worse one).
                metrics::counter!(
                    "tv_tick_flush_queue_full_total",
                    "feed" => self.feed.as_str()
                )
                .increment(1);
                let held = returned.buffer.as_bytes().len();
                self.buffer = returned.buffer;
                self.retained_spans = self.retained_spans.saturating_add(1);
                // TWO independent cuts, and the SPAN one is the reason this
                // change is safe to ship at all.
                //
                // Span: commit WIDTH is the measured amplifier — 10% of a
                // day's ticks carry an exchange timestamp over an hour behind
                // arrival, so a wide commit reopens closed hourly partitions
                // and rewrites them. Accumulating without this bound would have
                // made the disk pressure this change exists to relieve WORSE.
                //
                // Bytes: a belt-and-braces bound on a pathological append rate,
                // kept well under the questdb-rs wedge (const-asserted above).
                // `>` and not `>=`: the constant names how many spans may be
                // RETAINED, so the cut belongs on the span after them.
                if self.retained_spans > MAX_RETAINED_FLUSH_SPANS
                    || held >= MAX_PRODUCER_BUFFER_BYTES
                {
                    metrics::counter!(
                        "tv_tick_flush_width_capped_total",
                        "feed" => self.feed.as_str()
                    )
                    .increment(1);
                    self.retained_spans = 0;
                    // Rescue rather than keep widening. Durable, counted, and
                    // re-ingestable — the same tier a failed flush uses.
                    let dropped = self.discard_pending();
                    return OffloadOutcome::WidthCapped(dropped);
                }
                OffloadOutcome::QueueFull(rows)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(returned)) => {
                // The writer thread died. Rescue rather than drop, and say so.
                self.buffer = returned.buffer;
                self.restore_pending_range((returned.min_seq, returned.max_seq));
                let dropped = self.discard_pending();
                OffloadOutcome::SinkGone(dropped)
            }
        }
    }

    /// Rescue the buffered rows to the spill tier instead of losing them.
    ///
    /// # Why this hands off instead of writing (2026-08-28)
    ///
    /// This is reached from `try_offload`'s `WidthCapped` and `SinkGone` arms,
    /// which run ON THE FRAME-DRAIN TASK. The write it used to perform inline
    /// is not small: `create_dir_all`, a `read_dir` + per-entry `metadata()`
    /// walk of the spill directory AND its quarantine subdirectory, a live
    /// free-space probe, and then up to [`MAX_PRODUCER_BUFFER_BYTES`] —
    /// 32 MiB — of file write. All of it on the same volume QuestDB is
    /// stalling on.
    ///
    /// CORRECTED 2026-09-01 (adversarial review): this said "a live `statvfs`".
    /// There is no `statvfs` anywhere in this workspace — the probe is
    /// [`crate::disk_health_watcher::probe_disk_free_bytes`], which **forks and
    /// execs `df`**. That is materially worse than the doc implied on exactly
    /// this path, since a fork is far more expensive than a syscall and this
    /// arm runs on the frame-drain task. The cost argument the paragraph makes
    /// is therefore stronger than it was written to be, not weaker.
    ///
    /// And it fires at the worst possible instant BY CONSTRUCTION. The cut
    /// that calls it only trips after the hand-off queue has been full for
    /// `MAX_RETAINED_FLUSH_SPANS` consecutive flushes — that is, only when the
    /// database is already not keeping up, which on this box means the disk is
    /// already wedged. So the rescue did its single biggest write, on the
    /// decoder thread, precisely when a write was slowest.
    ///
    /// That is the same coupling the 2026-08-25 tick split and the 2026-08-28
    /// depth split were built to remove, one layer further in: a stalled drain
    /// stops emptying the socket, the receive buffer fills, and Dhan — which
    /// skips a slow consumer forward to "the latest available state" with no
    /// sequence number — discards the intermediate ticks at THEIR side, where
    /// no counter of ours can see them.
    ///
    /// So the buffer is now handed to a dedicated rescue thread by pointer.
    /// The fallback is deliberately the OLD behaviour, never a drop: if the
    /// rescue queue is full or its thread is gone, the write happens inline
    /// exactly as before. A slow drain is bad; a lost tick is worse.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped == 0 {
            self.buffer.clear();
            self.pending = 0;
            return 0;
        }

        // Off-drain hand-off. O(1), no syscall, no allocation: the `Buffer` is
        // MOVED, and the replacement is the same empty one `offload_flush`
        // already installs on every successful hand-off.
        let range = self.take_pending_range();
        if let Some(tx) = self.rescue.as_ref() {
            let protocol = self.buffer.protocol_version();
            let batch = RescueBatch {
                buffer: std::mem::replace(&mut self.buffer, Buffer::new(protocol)),
                rows: dropped,
                min_seq: range.0,
                max_seq: range.1,
            };
            match tx.try_send(batch) {
                Ok(()) => {
                    metrics::counter!(
                        TICK_RESCUE_QUEUED_COUNTER,
                        "feed" => self.feed.as_str()
                    )
                    .increment(dropped as u64);
                    // Counted like a writer hand-off, so a replay confirm waits
                    // for the rescue thread too — a payload in THIS queue is
                    // not yet in any file.
                    crate::wal_applied_watermark::applied_watermark().note_ticks_handed_off();
                    self.pending = 0;
                    return dropped;
                }
                Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                    // The rescue thread is behind. Take the buffer BACK and
                    // write inline below — slower, but nothing is lost and
                    // nothing is reported as lost.
                    self.buffer = returned.buffer;
                    metrics::counter!(
                        TICK_RESCUE_INLINE_FALLBACK_COUNTER,
                        "feed" => self.feed.as_str(),
                        "reason" => "queue_full"
                    )
                    .increment(1);
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(returned)) => {
                    self.buffer = returned.buffer;
                    metrics::counter!(
                        TICK_RESCUE_INLINE_FALLBACK_COUNTER,
                        "feed" => self.feed.as_str(),
                        "reason" => "thread_gone"
                    )
                    .increment(1);
                }
            }
        }

        let landed =
            perform_tick_rescue(&self.spill_dir, self.buffer.as_bytes(), self.feed, dropped);
        note_rescue_outcome_ticks(landed, range, false);
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

/// A rescued payload is APPLIED from the WAL's point of view — the spill tier
/// is durable and re-ingestable — and a failed rescue is the one arm where
/// captured frames genuinely need the next replay.
/// `in_order` is `true` ONLY on the writer thread, which completes batches in
/// the order they were handed off. A rescue from the PRODUCER (inline
/// fallback) or the rescue THREAD can land while earlier batches still sit in
/// the writer's queue; acking its `max_seq` there would lift the watermark
/// over rows that are in neither QuestDB nor a spill file — and a crash in
/// that window (the 2026-09-02 OOM shape) would archive their segments
/// unread next boot. Out of order, a landing acks nothing: those frames are
/// re-replayed and collapse on DEDUP, which is the cheap direction.
fn note_rescue_outcome_ticks(landed: bool, range: (u64, u64), in_order: bool) {
    let wm = crate::wal_applied_watermark::applied_watermark();
    if landed {
        if in_order {
            wm.note_ticks_acked(range.1);
        }
    } else {
        wm.note_unapplied_range(range.0, range.1);
        wm.note_unlanded();
    }
}

/// The rescue write itself — the part that touches the disk.
///
/// Extracted 2026-08-28 so the SAME code serves both the dedicated rescue
/// thread and the inline fallback in [`TickPersistenceWriter::discard_pending`].
/// Two copies would have drifted, and the copy that drifted would have been the
/// fallback — the one that only runs on the worst day.
fn perform_tick_rescue(spill_dir: &Path, payload: &[u8], feed: Feed, dropped: usize) -> bool {
    let payload_len = payload.len();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    match spill_failed_ilp(spill_dir, payload, feed, now) {
        Ok(path) => {
            // BOTH counters, and the alarmed one is not optional.
            //
            // `tv_ticks_dropped_total` is EMF-selected and carries the
            // `dhan_ticks_dropped` alarm (`live-lane-alarms.tf`).
            // `tv_ticks_spilled_total` carries NEITHER. Incrementing only the
            // new name would have DIVERTED the common flush failure — the exact
            // 2026-08-21 timeout — off the only pager that watches it, so the
            // operator would have been told less than before the rescue
            // existed. That is a false-OK (audit Rule 11), and a rescue that
            // blinds the alarm is a worse outcome than the loss it prevents.
            //
            // The alarmed counter is also the SEMANTICALLY correct one here: it
            // means "rows left the buffer without reaching QuestDB", which is
            // TRUE of a rescued row — the file is on disk, the database does
            // not have it. `spilled` is the strictly narrower fact "and it is
            // recoverable", which is why it is a second increment rather than a
            // replacement.
            metrics::counter!("tv_ticks_dropped_total", "feed" => feed.as_str())
                .increment(dropped as u64);
            metrics::counter!("tv_ticks_spilled_total", "feed" => feed.as_str())
                .increment(dropped as u64);
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                feed = feed.as_str(),
                rescued = dropped,
                bytes = payload_len,
                path = %path.display(),
                "tick flush failed — the buffered rows were RESCUED to the tick \
                 spill file named here, not lost. They are NOT in QuestDB yet. \
                 Re-ingest is one command and is safe to repeat, because the \
                 ticks dedup key carries capture_seq: \
                 curl --data-binary @<path> http://<questdb>:9000/write"
            );
            true
        }
        Err(err) => {
            // The rescue itself failed (disk full, cap reached, no
            // permission). Fall back to the counted drop — a spill that
            // cannot be written must never mask the loss.
            metrics::counter!("tv_ticks_dropped_total", "feed" => feed.as_str())
                .increment(dropped as u64);
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                feed = feed.as_str(),
                dropped,
                spill_error = %err,
                "tick flush failed AND the spill rescue also failed — these ticks \
                 are permanently lost and nothing re-inserts them. The raw frames \
                 remain in the write-ahead log for manual recovery."
            );
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Off-drain RESCUE (2026-08-28)
// ---------------------------------------------------------------------------

/// Depth of the hand-off queue between the drain and the rescue thread.
///
/// TWO, and small on purpose. A rescue only happens when the flush queue has
/// already been full for [`MAX_RETAINED_FLUSH_SPANS`] consecutive flushes, so
/// this queue is not a buffer for a busy day — it is a place to put ONE
/// oversized payload while the previous one is being written. Anything deeper
/// would hold more rows in memory that exist nowhere else, which is the trade
/// the whole rescue tier exists to avoid.
pub const RESCUE_QUEUE_DEPTH: usize = 2;

/// Rows handed to the rescue thread rather than written on the drain.
///
/// NOT a loss counter. It says the rescue was queued; `tv_ticks_spilled_total`
/// (incremented by the thread) says it landed. The gap between them is the
/// window a crash would take, which is why the queue is two deep.
pub const TICK_RESCUE_QUEUED_COUNTER: &str = "tv_tick_rescue_queued_total";

/// Rescues that had to be written INLINE on the drain after all.
///
/// Non-zero means the drain took the stall this hand-off exists to remove —
/// either the rescue thread was behind (`queue_full`) or it was gone
/// (`thread_gone`). Not a loss: nothing is dropped on either arm.
pub const TICK_RESCUE_INLINE_FALLBACK_COUNTER: &str = "tv_tick_rescue_inline_fallback_total";

/// Rescue payloads abandoned because the rescue thread did not finish.
///
/// These rows were counted as rescued by the producer and never reached the
/// spill file, so they are a REAL loss — the WAL-shutdown class, one tier out.
pub const TICK_RESCUE_ABANDONED_COUNTER: &str = "tv_tick_rescue_abandoned_total";

/// One oversized ILP payload on its way to the spill tier.
///
/// Carries the `Buffer` by move, so the hand-off costs a pointer write on the
/// drain rather than a copy of up to [`MAX_PRODUCER_BUFFER_BYTES`].
pub struct RescueBatch {
    buffer: Buffer,
    rows: usize,
    /// Lowest / highest `capture_seq` in this payload (`0` = unknown).
    min_seq: u64,
    max_seq: u64,
}

impl RescueBatch {
    /// Rows this payload carries — the number an abandoned batch loses.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }
}

/// The rescue thread's half: everything the write needs, and nothing else.
///
/// Deliberately owns no `Sender` and no `Buffer` — it cannot flush to QuestDB
/// and it cannot be confused with [`TickWriterSink`]. Its only job is to turn
/// a queued payload into a named file on disk.
pub struct TickRescueSink {
    spill_dir: PathBuf,
    feed: Feed,
}

impl TickRescueSink {
    /// Writes one queued payload to the spill tier.
    ///
    /// Identical code to the inline fallback — same counters, same coded
    /// error, same one-command recovery text — because both call
    /// [`perform_tick_rescue`]. An operator cannot tell which path ran, and
    /// should not have to.
    pub fn rescue(&self, batch: &RescueBatch) {
        let landed = perform_tick_rescue(
            &self.spill_dir,
            batch.buffer.as_bytes(),
            self.feed,
            batch.rows,
        );
        note_rescue_outcome_ticks(landed, (batch.min_seq, batch.max_seq), false);
        // The hand-off was counted when the producer queued this payload; the
        // replay confirm waits for this completion like any writer batch.
        crate::wal_applied_watermark::applied_watermark().note_ticks_completed();
    }
}
// ---------------------------------------------------------------------------
// Off-drain flush (2026-08-25)
// ---------------------------------------------------------------------------

/// Depth of the hand-off queue between the drain and the writer thread.
///
/// FOUR, not "large". The queue is a shock absorber for a QuestDB hiccup that
/// is SHORTER than the flush cadence, not a place to store data: every batch
/// sitting in it is rows that exist only in this process's memory, so a deep
/// queue converts a database stall into a bigger crash-loss window while
/// making the operator's counters look calmer. At the 500 ms flush timer this
/// absorbs ~2 s of stall, which is the class of blip the drain used to eat
/// synchronously; anything longer SHOULD show up as backpressure, because it
/// is one.
pub const FLUSH_QUEUE_DEPTH: usize = 4;

/// How much un-handed-off ILP text the PRODUCER may hold before it rescues.
///
/// When the queue is full the drain keeps its buffer and keeps appending —
/// that is the whole point, the rows are not lost and not reported as lost.
/// But "keep appending forever" is an unbounded memory path, and this file
/// exists in a repo whose complexity table has now recorded five uncapped
/// maps. So past this ceiling the producer stops accumulating and rescues to
/// the spill tier, which is durable, counted, and re-ingestable — the same
/// tier a failed flush uses.
///
/// This is the SECONDARY bound. [`MAX_RETAINED_FLUSH_SPANS`] is the primary
/// one and cuts far earlier; this exists so a pathological append rate cannot
/// reach the wedge below even inside two spans.
///
/// 32 MiB and not 64: the first draft used 64, and the const assertion below
/// REFUSED TO COMPILE, which is the assertion doing its job. With a batch
/// already in flight toward a client whose buffer wedges permanently at
/// 100 MiB, a 64 MiB producer ceiling leaves no real headroom.
pub const MAX_PRODUCER_BUFFER_BYTES: usize = 32 * 1024 * 1024;

/// The questdb-rs client buffer ceiling. Past it EVERY flush fails, permanently
/// — a wedge, not a degrade, which is why the producer must cut well below it.
///
/// Named here rather than left implicit because the two sibling writers
/// (`seal_writer_task.rs`, `shadow_candle_writer.rs`) both document this cliff
/// in prose and neither asserts against it. A number that only exists in a
/// comment is a number nothing checks.
pub const QUESTDB_MAX_BUF_SIZE_BYTES: usize = 100 * 1024 * 1024;

// The producer ceiling must leave real headroom under the wedge, or the
// "rescue instead of accumulate" arm fires only after every flush is already
// permanently failing — which would make the rescue path unreachable exactly
// when it is needed.
const _: () = assert!(
    MAX_PRODUCER_BUFFER_BYTES * 2 <= QUESTDB_MAX_BUF_SIZE_BYTES,
    "the producer ceiling must sit at or below half the questdb-rs max_buf_size wedge"
);

/// How many consecutive 500 ms flush spans the producer may RETAIN before it
/// stops accumulating and spills.
///
/// # This is the condition the measurement made load-bearing
///
/// A design pass on 2026-08-25 flagged an own-goal that the obvious version of
/// this change walks straight into: a decoupled writer batches more
/// aggressively under pressure, so each commit spans a WIDER range of rows —
/// and wider commits are exactly what the write amplification is made of. The
/// same change is therefore beneficial or harmful depending on which amplifier
/// is real, and the design bound the implementing PR to cap batch width "at
/// today's 500 ms span" until that was measured.
///
/// It was then measured on the live box: `ticks` is `PARTITION BY HOUR` on the
/// exchange last-TRADE time, and **10.0% of one day's 64.3M ticks carried a
/// `ts` more than an hour behind arrival** — legitimately, because for an
/// illiquid strike the last trade genuinely was hours ago. So one commit in ten
/// reopens an already-closed hourly partition and REWRITES it. Commit width is
/// the amplifier. Unbounded accumulation would have made the disk problem this
/// change exists to relieve measurably worse.
///
/// TWO, not one: the queue already absorbs `FLUSH_QUEUE_DEPTH` batches before
/// the producer ever sees a full queue, so the honest absorption is that depth
/// plus this, and a cap of one would spill on the first hiccup. Two keeps the
/// widest possible commit to roughly three flush spans rather than the ~128
/// that the byte ceiling alone would have permitted at a typical row size.
pub const MAX_RETAINED_FLUSH_SPANS: u32 = 2;

/// One handed-off ILP payload, in flight between the drain and the writer.
///
/// Deliberately opaque: the drain must not be able to inspect, re-order, or
/// partially consume a batch, because the only correct thing to do with it is
/// hand it to the network or rescue the whole thing to disk.
pub struct FlushBatch {
    buffer: Buffer,
    rows: usize,
    /// Lowest / highest `capture_seq` in this batch (`0` = unknown).
    min_seq: u64,
    max_seq: u64,
}

impl FlushBatch {
    /// Rows this batch covers.
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the offload tests below.
    pub const fn rows(&self) -> usize {
        self.rows
    }
}

/// What happened to a batch the producer tried to hand off.
///
/// Three arms and not a `Result`, because the middle one is NOT a failure and
/// must never be logged or counted as one: a full queue means the rows are
/// still held, still pending, and will go out on the next flush. Collapsing it
/// into `Err` is precisely how a backpressure signal becomes a false loss
/// report.
#[derive(Debug, PartialEq, Eq)]
pub enum OffloadOutcome {
    /// Handed to the writer thread. The rows are no longer this side's.
    Sent(usize),
    /// The writer is behind. Rows RETAINED by the producer, nothing lost.
    QueueFull(usize),
    /// The writer stayed behind long enough that retaining further would
    /// WIDEN the commit past [`MAX_RETAINED_FLUSH_SPANS`]. Rows rescued to the
    /// spill tier rather than accumulated.
    ///
    /// Its own arm and not `SinkGone`, because the writer is alive and well —
    /// reporting "the writer thread is gone" here would send an operator to
    /// diagnose a thread that is running fine.
    WidthCapped(usize),
    /// The writer thread is gone. Rows rescued to the spill tier.
    SinkGone(usize),
}

/// The network half of a split [`TickWriter`] — owns the ILP `Sender`.
///
/// Lives on its own OS thread. It never touches the aggregator, the ring, or
/// anything the drain owns, which is the entire reason the split exists: a
/// five-second ILP timeout now blocks a thread whose only job is waiting, not
/// the thread that must keep emptying the socket.
pub struct TickWriterSink {
    sender: Option<Sender>,
    feed: Feed,
    spill_dir: PathBuf,
}

impl TickWriterSink {
    /// Writes one batch. Returns the rows that actually LANDED in QuestDB.
    ///
    /// Zero on any failure — the same contract `TickWriter::flush` has, and
    /// for the same reason: the caller reports feed health from this number,
    /// so a failed write must decay health rather than forge it.
    ///
    /// A failure rescues the payload to the spill tier through the identical
    /// path `discard_pending` uses (same two counters, same coded error, same
    /// one-command recovery), so an operator sees no difference between a
    /// synchronous and an offloaded rescue.
    pub fn write(&mut self, batch: &mut FlushBatch) -> usize {
        let landed = self.write_inner(batch);
        // The batch is DONE — landed or rescued — whatever the outcome. This
        // is what the replay confirm waits on, and the watermark file is
        // refreshed from here at most once a second, off the drain.
        let wm = crate::wal_applied_watermark::applied_watermark();
        wm.note_ticks_completed();
        wm.persist_if_due_now();
        landed
    }

    fn write_inner(&mut self, batch: &mut FlushBatch) -> usize {
        if batch.rows == 0 {
            return 0;
        }
        let Some(sender) = self.sender.as_mut() else {
            self.rescue(batch, "no ILP sender (QuestDB unreachable)");
            return 0;
        };
        // 2026-09-03: a bounded retry, the shape `DepthWriter` has carried on
        // its synchronous path since 2026-08-25. The TICK path has never had
        // one — on either path — and the measured cost of that is the largest
        // single loss signal in the session.
        //
        // MEASURED 2026-09-03: `HOT-PATH-02` fired **58,746 times**, every one
        // reading `Could not flush buffer: ... io: Connection reset by peer
        // (os error 104)`. Each rescued ~120 rows to the spill tier, so the
        // great majority of the session's ticks are sitting in files on disk
        // rather than in QuestDB — `SELECT count() FROM ticks WHERE ts IN
        // today()` read 2,310,693 against roughly 82M ingested. Nothing was
        // LOST; almost nothing was QUERYABLE.
        //
        // A reset is the transport class the buffer survives: the rows are
        // still in `batch.buffer` (which is exactly why `rescue` below can
        // spill them), so a second attempt sends the same bytes.
        //
        // Retrying is idempotent BY CONSTRUCTION, which is what makes one
        // attempt safe rather than merely cheap: `DEDUP_KEY_TICKS` carries
        // `capture_seq`, unique per received frame. The rescue message three
        // dozen lines below says so in the operator's own words — "safe to
        // repeat, because the ticks dedup key carries capture_seq". If it is
        // safe for the operator to re-send by hand, it is safe for us to
        // re-send once automatically.
        //
        // Bounded at ONE, and gated on the first failure being FAST.
        // `request_timeout` is 5,000 ms, so an unconditional retry makes the
        // worst case ten seconds. This thread is not the drain — that is the
        // whole point of the split — so a stall here does not fill the socket
        // buffer directly. It fills the hand-off QUEUE instead, and a full
        // queue makes the producer rescue rows the retry existed to save. A
        // reset returns in microseconds; a timeout consumes the whole budget.
        let started = std::time::Instant::now();
        let first = sender.flush(&mut batch.buffer);
        let first_elapsed = started.elapsed();
        let outcome = match first {
            Ok(()) => Ok(()),
            Err(err)
                if crate::depth_persistence::flush_failure_is_retryable(&err)
                    && first_elapsed
                        < crate::depth_persistence::DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW =>
            {
                metrics::counter!(
                    "tv_tick_flush_retries_total",
                    "feed" => self.feed.as_str(),
                )
                .increment(1);
                sender.flush(&mut batch.buffer)
            }
            Err(err) => {
                if crate::depth_persistence::flush_failure_is_retryable(&err) {
                    // Retryable in class but SLOW: not retried, and counted so
                    // "the retry is not firing" is answerable without a guess.
                    metrics::counter!(
                        "tv_tick_flush_retries_skipped_total",
                        "feed" => self.feed.as_str(),
                    )
                    .increment(1);
                }
                Err(err)
            }
        };
        match outcome {
            Ok(()) => {
                let landed = batch.rows;
                batch.rows = 0;
                // ACKED by QuestDB: every captured frame up to this sequence
                // has its rows in the database.
                crate::wal_applied_watermark::applied_watermark().note_ticks_acked(batch.max_seq);
                landed
            }
            Err(err) => {
                let why = format!("{err}");
                self.rescue(batch, &why);
                0
            }
        }
    }

    /// Rescues a batch the network refused, exactly as `discard_pending` does.
    fn rescue(&mut self, batch: &mut FlushBatch, why: &str) {
        let rows = batch.rows;
        if rows == 0 {
            return;
        }
        let payload_len = batch.buffer.as_bytes().len();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        match spill_failed_ilp(&self.spill_dir, batch.buffer.as_bytes(), self.feed, now) {
            Ok(path) => {
                note_rescue_outcome_ticks(true, (batch.min_seq, batch.max_seq), true);
                metrics::counter!("tv_ticks_dropped_total", "feed" => self.feed.as_str())
                    .increment(rows as u64);
                metrics::counter!("tv_ticks_spilled_total", "feed" => self.feed.as_str())
                    .increment(rows as u64);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    feed = self.feed.as_str(),
                    rescued = rows,
                    bytes = payload_len,
                    reason = why,
                    path = %path.display(),
                    "offloaded tick flush failed — the rows were RESCUED to the tick \
                     spill file named here, not lost. They are NOT in QuestDB yet. \
                     Re-ingest is one command and is safe to repeat, because the \
                     ticks dedup key carries capture_seq: \
                     curl --data-binary @<path> http://<questdb>:9000/write"
                );
            }
            Err(err) => {
                note_rescue_outcome_ticks(false, (batch.min_seq, batch.max_seq), true);
                metrics::counter!("tv_ticks_dropped_total", "feed" => self.feed.as_str())
                    .increment(rows as u64);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    feed = self.feed.as_str(),
                    dropped = rows,
                    reason = why,
                    spill_error = %err,
                    "offloaded tick flush failed AND the spill rescue also failed — these \
                     ticks are permanently lost and nothing re-inserts them. The raw frames \
                     remain in the write-ahead log for manual recovery."
                );
            }
        }
        batch.buffer.clear();
        batch.rows = 0;
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
            average_traded_price: 23_145.1,
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

    /// 2026-08-21 — the NaN chain, closed at the source.
    ///
    /// `parse_quote_packet` is proven by its OWN test to propagate
    /// `f32::NAN` into `day_open/high/low/close`, and IDX_I moved to Quote
    /// mode on 2026-08-21, so this is a live wire shape and not a
    /// hypothetical. The pre-fix `opt_price` gate was `(v != 0.0).then(..)`,
    /// which NaN PASSES (NaN compares unequal to everything), and
    /// `f32_to_f64_clean` returns non-finite unchanged — so a NaN reached
    /// the ILP line, QuestDB rejected the whole batch, the rescued buffer
    /// was spilled, and `tick_spill_replay` then retried a file it can never
    /// accept FIRST in every round (files are sorted; this tier has no DLQ).
    ///
    /// BITE PROOF: delete the `is_finite` loop in `from_parsed_tick` and
    /// this test fails — the row is built and `high` comes back `Some(NaN)`.
    /// `spill_free_bytes` collapses the probe outcome to the one bit the
    /// decision needs.
    ///
    /// Deliberately asserts a RANGE rather than a value: the number is
    /// whatever the host has, and a test that pins a specific free-byte count
    /// is a test that fails on a different machine. What must hold everywhere
    /// is that a readable directory yields `Some`, and that the value is a
    /// plausible byte count rather than a sentinel.
    #[test]
    fn test_spill_free_bytes_reads_a_real_directory() {
        let dir = std::env::temp_dir();
        let free = spill_free_bytes(&dir);
        let Some(bytes) = free else {
            // A probe failure on a real temp dir is itself worth failing on:
            // it would mean every rescue past the soft rail refuses, which is
            // the pre-fix behaviour wearing a different hat.
            panic!("the system temp dir must be probeable; got None");
        };
        assert!(
            bytes > 0,
            "a mounted filesystem reporting zero free bytes would refuse every \
             rescue — if this ever fires, check the probe, not the disk"
        );
        // A path that cannot exist must degrade to None, never to a number.
        let missing = std::path::Path::new("/nonexistent-tv-spill-probe-target");
        assert_eq!(
            spill_free_bytes(missing),
            None,
            "an unreadable path must yield None so the classifier REFUSES — \
             inventing a number here would license unbounded growth"
        );
    }

    /// The rule itself, exhaustively, with no filesystem involved.
    ///
    /// This is the arm coverage the old shape could never have: a unit test
    /// cannot manufacture a nearly-full disk, so before the decision was
    /// separated out, the arm that protects the database was untestable and
    /// Every arm of the per-write floor, including the two the live box can
    /// only reach with a broken `df`.
    #[test]
    fn classify_spill_floor_covers_every_arm() {
        use BlindWritePolicy::{FailClosed, FailOpen};
        use SpillFloorVerdict as V;
        for (free, payload, floor, blind, want) in [
            // Known free space: identical for both policies.
            (Some(100_u64), 1_u64, 10_u64, FailOpen, V::Allow),
            (Some(100), 1, 10, FailClosed, V::Allow),
            (Some(10), 1, 10, FailOpen, V::RefuseNoRoom),
            (Some(10), 1, 10, FailClosed, V::RefuseNoRoom),
            // Exactly enough is enough — the floor is inclusive.
            (Some(11), 1, 10, FailClosed, V::Allow),
            // Blind: the policies diverge, and that divergence is the fix.
            (None, 1, 10, FailOpen, V::AllowProbeFailed),
            (None, 1, 10, FailClosed, V::RefuseProbeFailed),
            // A zero floor does NOT re-open a fail-closed tier when the probe
            // is blind. The carve-out that used to allow this was removed on
            // 2026-09-01: the policy is about BLINDNESS, not about the floor
            // value, and `DepthWriter::for_test` (pub, floor 0) is reachable
            // from a production `mem::replace` placeholder — so the carve-out
            // was a live path back to blind writing for the one tier that
            // must never take it.
            (None, 1, 0, FailClosed, V::RefuseProbeFailed),
            // Saturating: a payload near u64::MAX must refuse, never WRAP
            // into a small `needed` that then reads as plenty of room.
            (
                Some(u64::MAX - 5),
                u64::MAX,
                10,
                FailClosed,
                V::RefuseNoRoom,
            ),
            // HONEST EDGE, asserted rather than hidden: at exactly u64::MAX
            // free AND a u64::MAX payload, `needed` saturates to u64::MAX and
            // the floor is swallowed, so this ALLOWS. It is unreachable — the
            // payload is a Vec that must fit in RAM — and it is pinned here
            // because the first draft of this test asserted the opposite and
            // was wrong. Saturation is still the right choice: the failure it
            // prevents is wrapping, which would allow a huge write on a nearly
            // full disk. This edge allows a huge write on an infinite one.
            (Some(u64::MAX), u64::MAX, 10, FailClosed, V::Allow),
        ] {
            assert_eq!(
                classify_spill_floor(free, payload, floor, blind),
                want,
                "free={free:?} payload={payload} floor={floor} blind={blind:?}"
            );
        }
    }

    /// THE invariant, swept across the whole domain including the blind case.
    ///
    /// Depth is record-only; ticks are decision-critical. So at every possible
    /// disk state, depth must refuse to spill AT LEAST as readily as ticks. A
    /// state where depth writes while ticks are refused is the inversion the
    /// two-reserve split exists to prevent.
    ///
    /// The 2026-09-01 sweep that found this bug found it in the `None` column,
    /// which the existing arithmetic guard cannot reach: that guard compares
    /// byte values, and probe failure carries no byte value at all. This test
    /// drives BOTH tiers' complete decision — ceiling arm and floor — over
    /// known AND unknown free space.
    #[test]
    fn depth_never_spills_while_ticks_are_refused_including_when_df_is_blind() {
        const GIB: u64 = 1024 * 1024 * 1024;

        fn tick_refuses(free: Option<u64>, dir: u64, ceiling: u64, payload: u64) -> bool {
            if matches!(
                classify_spill_ceiling(dir, ceiling, free, SPILL_SOFT_CEILING_FREE_RESERVE_BYTES),
                SpillCeilingVerdict::OverCeilingNoRoom
                    | SpillCeilingVerdict::OverCeilingProbeFailed
            ) {
                return true;
            }
            matches!(
                classify_spill_floor(
                    free,
                    payload,
                    SPILL_MIN_FREE_HEADROOM_BYTES,
                    BlindWritePolicy::FailOpen,
                ),
                SpillFloorVerdict::RefuseNoRoom | SpillFloorVerdict::RefuseProbeFailed
            )
        }

        fn depth_refuses(free: Option<u64>, dir: u64, cap: u64, payload: u64) -> bool {
            if matches!(
                classify_spill_ceiling(dir, cap, free, DEPTH_SPILL_FREE_RESERVE_BYTES),
                SpillCeilingVerdict::OverCeilingNoRoom
                    | SpillCeilingVerdict::OverCeilingProbeFailed
            ) {
                return true;
            }
            matches!(
                classify_spill_floor(
                    free,
                    payload,
                    DEPTH_SPILL_MIN_FREE_HEADROOM_BYTES,
                    BlindWritePolicy::FailClosed,
                ),
                SpillFloorVerdict::RefuseNoRoom | SpillFloorVerdict::RefuseProbeFailed
            )
        }

        let frees = [
            None,
            Some(0),
            Some(1),
            Some(GIB),
            Some(SPILL_SOFT_CEILING_FREE_RESERVE_BYTES),
            Some(SPILL_SOFT_CEILING_FREE_RESERVE_BYTES + 1),
            Some(DEPTH_SPILL_FREE_RESERVE_BYTES),
            Some(DEPTH_SPILL_FREE_RESERVE_BYTES + 1),
            Some(200 * GIB),
        ];
        // The directories are INDEPENDENT, which is exactly what made the bug
        // reachable — so they are swept independently, both over and under
        // their own rails.
        let dirs = [0_u64, 5 * GIB, 10 * GIB];
        let rails = [1_u64, 10 * GIB];
        // Payload >= 1. A zero-byte spill is excluded deliberately: it writes
        // nothing, so it cannot consume the room ticks need, and treating it
        // as an inversion would be an assertion about a no-op.
        let payloads = [1_u64, 4096, 32 * 1024 * 1024];

        for free in frees {
            for td in dirs {
                for tc in rails {
                    for dd in dirs {
                        for dc in rails {
                            // ASYMMETRIC payloads (2026-09-01, adversarial review).
                            // The sweep used to bind ONE `p` to both tiers,
                            // which asserts a weaker invariant than the one
                            // that matters: the real requirement is that a
                            // refused tick implies a refused depth write for
                            // ANY depth payload, not only an equal-sized one.
                            // Bounded in practice by the 32 MiB producer caps,
                            // so the asymmetry is unreachable today — but a
                            // future floor change would have passed the old
                            // test while a real inversion existed.
                            for tp in payloads {
                                for dp in payloads {
                                    if tick_refuses(free, td, tc, tp) {
                                        assert!(
                                            depth_refuses(free, dd, dc, dp),
                                            "INVERSION: ticks refused but depth would write. \
                                             free={free:?} tick_dir={td} tick_ceiling={tc} \
                                             tick_payload={tp} depth_dir={dd} depth_cap={dc} \
                                             depth_payload={dp}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The precise state that was broken, named so a regression is legible
    /// rather than one failing row inside the sweep above.
    #[test]
    fn a_blind_probe_refuses_depth_even_while_its_own_directory_is_under_cap() {
        // Depth's directory is well under its cap, so its ceiling arm is
        // skipped entirely — this is the regime depth spends most of its life
        // in, and before the fix it wrote blind here.
        assert_eq!(
            classify_spill_ceiling(
                0,
                10 * 1024 * 1024 * 1024,
                None,
                DEPTH_SPILL_FREE_RESERVE_BYTES
            ),
            SpillCeilingVerdict::UnderCeiling,
            "precondition: depth's ceiling arm must be skipped for this to test the floor"
        );
        assert_eq!(
            classify_spill_floor(
                None,
                4096,
                DEPTH_SPILL_MIN_FREE_HEADROOM_BYTES,
                BlindWritePolicy::FailClosed,
            ),
            SpillFloorVerdict::RefuseProbeFailed,
            "depth must refuse when it cannot see the disk"
        );
        // The tick tier in the SAME state still writes — that asymmetry is the
        // point, not an oversight.
        assert_eq!(
            classify_spill_floor(
                None,
                4096,
                SPILL_MIN_FREE_HEADROOM_BYTES,
                BlindWritePolicy::FailOpen,
            ),
            SpillFloorVerdict::AllowProbeFailed,
            "ticks must keep writing: one broken df must not disable the \
             decision-critical tier"
        );
    }

    /// the arm that lost 243 million rows was the one pinned.
    #[test]
    fn test_classify_spill_ceiling_covers_every_arm() {
        const R: u64 = SPILL_SOFT_CEILING_FREE_RESERVE_BYTES;
        for (held, ceiling, free, want) in [
            (0_u64, 1_u64, None, SpillCeilingVerdict::UnderCeiling),
            (0, 1, Some(0), SpillCeilingVerdict::UnderCeiling),
            (1, 1, None, SpillCeilingVerdict::OverCeilingProbeFailed),
            (1, 1, Some(0), SpillCeilingVerdict::OverCeilingNoRoom),
            (1, 1, Some(R), SpillCeilingVerdict::OverCeilingNoRoom),
            (1, 1, Some(R + 1), SpillCeilingVerdict::OverCeilingWithRoom),
            (
                u64::MAX,
                0,
                Some(u64::MAX),
                SpillCeilingVerdict::OverCeilingWithRoom,
            ),
        ] {
            assert_eq!(
                classify_spill_ceiling(held, ceiling, free, R),
                want,
                "held={held} ceiling={ceiling} free={free:?} reserve={R}"
            );
        }
        // A ceiling of 0 means "always past the rail", so the decision is
        // then entirely a free-space question. Stated because a future
        // `tick_spill_max_bytes()` returning 0 on an unreadable volume must
        // not become an accidental allow-everything.
        assert_eq!(
            classify_spill_ceiling(0, 0, None, R),
            SpillCeilingVerdict::OverCeilingProbeFailed,
            "a zero ceiling with an unreadable probe must REFUSE, not allow"
        );
    }

    #[test]
    fn a_non_finite_price_is_refused_not_emitted_as_a_poison_ilp_row() {
        for (field, mutate) in [
            (
                "ltp",
                (|t: &mut ParsedTick| t.last_traded_price = f32::NAN) as fn(&mut ParsedTick),
            ),
            ("open", |t: &mut ParsedTick| t.day_open = f32::NAN),
            ("high", |t: &mut ParsedTick| t.day_high = f32::NAN),
            ("low", |t: &mut ParsedTick| t.day_low = f32::NAN),
            ("close", |t: &mut ParsedTick| t.day_close = f32::NAN),
            ("high", |t: &mut ParsedTick| t.day_high = f32::INFINITY),
            ("low", |t: &mut ParsedTick| t.day_low = f32::NEG_INFINITY),
        ] {
            let mut tick = sample_tick();
            mutate(&mut tick);
            let err = TickRow::from_parsed_tick(&tick, 1)
                .expect_err("a non-finite price MUST be refused, never built into a row");
            assert_eq!(
                err,
                TickRowError::PriceNotFinite { field },
                "the refusal must name the offending column"
            );
        }
    }

    /// The zero sentinel is NOT non-finite and must still be accepted — a
    /// Ticker (16-byte) packet carries only LTP + LTT, so `0.0` legitimately
    /// means "not carried" and becomes NULL. Refusing it would silently drop
    /// every Ticker-mode instrument, which is strictly worse than the bug
    /// being fixed.
    #[test]
    fn the_zero_not_carried_sentinel_is_still_accepted() {
        let mut tick = sample_tick();
        tick.day_open = 0.0;
        tick.day_high = 0.0;
        tick.day_low = 0.0;
        tick.day_close = 0.0;
        let row = TickRow::from_parsed_tick(&tick, 1)
            .expect("0.0 is the documented not-carried sentinel, not a bad price");
        assert!(row.open.is_none(), "0.0 must still become NULL");
        assert!(row.high.is_none());
        assert!(row.low.is_none());
        assert!(row.close.is_none());
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
        for raw in [23_925.65_f32, 23_937.3, 10.20, 25_461.3] {
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
        for (feed, label) in [(Feed::Dhan, "dhan"), (Feed::Truedata, "truedata")] {
            let mut w = TickWriter::for_test(feed);
            w.append_row(&sample_row()).expect("append");
            assert!(
                w.buffer_utf8().contains(&format!(",feed={label}")),
                "writer must stamp feed={label}"
            );
        }
        assert_eq!(TICK_FEED_DHAN, "dhan");
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

    // -- tick ILP spill rescue (2026-08-21) ---------------------------------

    /// Unique scratch dir per test — no `tempfile` dep (adding one needs
    /// operator approval per CLAUDE.md's dependency rule).
    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("tv-tick-spill-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // -----------------------------------------------------------------
    // Off-drain flush (2026-08-25)
    // -----------------------------------------------------------------

    #[test]
    fn offloaded_flush_hands_the_rows_off_and_does_not_touch_the_network() {
        // The whole point: a flush on the drain side must complete without a
        // network round trip. `for_test` has no sender at all, so if the
        // offload branch were skipped this would take the "QuestDB
        // unreachable" arm, rescue to disk, and return Err.
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_tick_with_seq(&sample_tick(), 1).expect("append");
        w.append_tick_with_seq(&sample_tick(), 2).expect("append");
        let (mut producer, _sink, rx) = w.split_for_offload();
        assert_eq!(producer.pending(), 2);

        producer.flush().expect("offloaded flush must not error");

        assert_eq!(
            producer.pending(),
            0,
            "the rows left the producer once handed off"
        );
        let batch = rx.try_recv().expect("the batch must be on the queue");
        assert_eq!(batch.rows(), 2, "the batch carries the row count verbatim");
    }

    #[test]
    fn a_full_queue_keeps_the_rows_and_never_reports_them_as_dropped() {
        // The arm that makes a BOUNDED queue safe. A full queue is
        // backpressure: the rows are still ours, still pending, and the next
        // flush retries. If this ever reported Err — or cleared `pending` —
        // the drain would either log a loss that did not happen or actually
        // lose the rows.
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_tick_with_seq(&sample_tick(), 1).expect("append");
        let (mut producer, _sink, _rx) = w.split_for_offload();

        // Fill the queue to its depth, holding the receiver so nothing drains.
        for i in 0..FLUSH_QUEUE_DEPTH {
            producer.flush().expect("flush while the queue has room");
            // Strictly increasing, as production's `next_capture_seq` is:
            // reusing one value would exercise a shape the live writer never
            // produces.
            let seq = 2 + i64::try_from(i).expect("loop bound fits i64");
            producer
                .append_tick_with_seq(&sample_tick(), seq)
                .expect("append");
        }
        assert_eq!(producer.pending(), 1);

        producer
            .flush()
            .expect("a full queue is backpressure, never an error");

        assert_eq!(
            producer.pending(),
            1,
            "the row is RETAINED — it is still ours to flush next time"
        );
    }

    #[test]
    fn the_producer_stops_widening_the_batch_and_spills_instead() {
        // THE condition the live measurement made load-bearing. `ticks` is
        // PARTITION BY HOUR on the exchange last-trade time, and 10% of a
        // day's ticks carry a ts over an hour behind arrival — so a wide
        // commit reopens closed hourly partitions and rewrites them. A
        // decoupled writer that accumulated without bound would have made the
        // write amplification WORSE, which is the own-goal a design pass
        // flagged before this was implemented.
        //
        // So: retaining is allowed, widening without limit is not.
        let dir = scratch_dir("offload-width-cap");
        let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        w.append_tick_with_seq(&sample_tick(), 1).expect("append");
        let (mut producer, mut sink, _rx) = w.split_for_offload();
        sink.spill_dir = dir.clone();

        // Fill the queue so every later flush is refused.
        for i in 0..FLUSH_QUEUE_DEPTH {
            producer.flush().expect("flush while the queue has room");
            let seq = 2 + i64::try_from(i).expect("loop bound fits i64");
            producer
                .append_tick_with_seq(&sample_tick(), seq)
                .expect("append");
        }

        // Retained spans, up to the cap — rows KEPT and accumulating, nothing
        // spilled. Accumulation is the point: these rows are still ours and
        // still pending, which is what makes backpressure lossless.
        for span in 0..MAX_RETAINED_FLUSH_SPANS {
            let before = producer.pending();
            producer.flush().expect("a full queue is backpressure");
            assert_eq!(
                producer.pending(),
                before,
                "span {span} must RETAIN its rows — a retained flush neither \
                 sends nor discards"
            );
            let seq = 100 + i64::from(span);
            producer
                .append_tick_with_seq(&sample_tick(), seq)
                .expect("append");
        }
        assert!(
            producer.pending() > 1,
            "retained spans accumulate — that is what makes backpressure lossless"
        );

        // One span past the cap: stop widening, spill instead.
        let capped = producer.flush();

        let msg = format!("{:#}", capped.expect_err("the width cap must report"));
        assert!(
            msg.contains("RESCUED to the spill tier"),
            "the message must say the rows are SAFE and where they went; got: {msg}"
        );
        assert!(
            !msg.contains("writer thread is gone"),
            "the writer is alive — reporting it gone would send an operator to \
             diagnose a healthy thread. Got: {msg}"
        );
        assert_eq!(
            producer.pending(),
            0,
            "the buffer was handed to the spill tier, so it is no longer pending"
        );
        assert!(
            std::fs::read_dir(&dir)
                .expect("spill dir")
                .filter_map(std::result::Result::ok)
                .count()
                >= 1,
            "the rows must be DURABLE on disk — capping width may never mean \
             dropping rows"
        );
    }

    #[test]
    fn the_sink_reports_zero_rows_when_the_flush_fails() {
        // The sink's row count is what feeds `record_ticks`. A failed write
        // must return 0 so feed health DECAYS; returning the batch size would
        // forge liveness during a database outage, which is the exact
        // false-OK `flush_and_record`'s own docstring warns about.
        let dir = scratch_dir("offload-sink-fail");
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_tick_with_seq(&sample_tick(), 1).expect("append");
        let (mut producer, mut sink, rx) = w.split_for_offload();
        sink.spill_dir = dir.clone();
        producer.flush().expect("hand off");

        let mut batch = rx.try_recv().expect("batch queued");
        // The sink has no sender (for_test), so the write must fail.
        let landed = sink.write(&mut batch);

        assert_eq!(landed, 0, "a failed write lands zero rows");
        let rescued: Vec<_> = std::fs::read_dir(&dir)
            .expect("spill dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(
            rescued.len(),
            1,
            "the payload was RESCUED to the spill tier, not dropped"
        );
    }

    /// The drain must not write the rescue file itself.
    ///
    /// This is the defect the split exists to remove: `discard_pending` used to
    /// do `create_dir_all`, a `read_dir` + per-entry `metadata()` walk of the
    /// spill directory AND its quarantine subdirectory, a live free-space probe
    /// (which forks `df` on its first call), and up to 32 MiB of file write —
    /// all on the frame-drain task, and all at the one moment the disk is
    /// already wedged, because the cut that calls it only trips when the flush
    /// queue has been full for several consecutive flushes.
    #[test]
    fn a_split_writer_hands_the_rescue_off_instead_of_writing_it() {
        let dir = scratch_dir("rescue-handoff");
        let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        let (_sink, rx) = w.split_rescue_offload();

        w.append_tick_with_seq(&sample_tick(), 1).expect("append");
        assert_eq!(w.pending(), 1);

        let rescued = w.discard_pending();

        assert_eq!(rescued, 1, "the rows are accounted for either way");
        assert_eq!(w.pending(), 0, "the producer must let go of them");
        let batch = rx
            .try_recv()
            .expect("the payload must be on the rescue queue, not on disk");
        assert_eq!(batch.rows(), 1);
        assert!(
            std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) == 0,
            "the drain must not have touched the spill directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A full rescue queue falls back to the OLD behaviour, never to a drop.
    ///
    /// The whole point of the fallback: a slow drain is bad, a lost tick is
    /// worse. If this ever became a refusal, the change would have traded a
    /// stall for exactly the loss it was built to prevent.
    #[test]
    fn a_full_rescue_queue_writes_inline_rather_than_dropping() {
        let dir = scratch_dir("rescue-full");
        let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        let (_sink, _rx) = w.split_rescue_offload();

        // Fill the queue: RESCUE_QUEUE_DEPTH payloads with nothing draining.
        for _ in 0..RESCUE_QUEUE_DEPTH {
            w.append_tick_with_seq(&sample_tick(), 1).expect("append");
            assert_eq!(w.discard_pending(), 1);
        }
        // The next one cannot be queued and must therefore land on disk.
        w.append_tick_with_seq(&sample_tick(), 1).expect("append");
        assert_eq!(w.discard_pending(), 1);

        let files = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        assert!(
            files > 0,
            "with the queue full the rescue must have been written inline"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A writer that was never split behaves exactly as before.
    #[test]
    fn an_unsplit_writer_still_rescues_inline() {
        let dir = scratch_dir("rescue-unsplit");
        let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        w.append_tick_with_seq(&sample_tick(), 1).expect("append");

        assert_eq!(w.discard_pending(), 1);
        assert!(
            std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) > 0,
            "no rescue channel means the old synchronous path, unchanged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Closing the rescue queue must leave the writer rescuing inline, not
    /// refusing — the end-of-session rows still have to reach the spill tier.
    #[test]
    fn closing_the_rescue_queue_restores_the_inline_path() {
        let dir = scratch_dir("rescue-closed");
        let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        let (_sink, _rx) = w.split_rescue_offload();
        w.close_rescue_offload();

        w.append_tick_with_seq(&sample_tick(), 1).expect("append");
        assert_eq!(w.discard_pending(), 1);
        assert!(
            std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) > 0,
            "after close the rescue must still land on disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The queue is deliberately shallow: every payload in it is rows that
    /// exist nowhere else, so depth here is a crash-loss window.
    #[test]
    fn the_rescue_queue_is_shallow_on_purpose() {
        assert!(
            RESCUE_QUEUE_DEPTH >= 1 && RESCUE_QUEUE_DEPTH <= 4,
            "one payload in flight plus a slot to hand the next one over; \
             deeper holds more rows that exist only in this process"
        );
    }

    #[test]
    fn a_writer_that_was_never_split_behaves_exactly_as_before() {
        // The offload is opt-in at ONE call site. Every other caller — and
        // every existing test — must be untouched, so a writer that was never
        // split still takes the synchronous no-sender arm and still rescues.
        let dir = scratch_dir("offload-unsplit");
        let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        w.append_tick_with_seq(&sample_tick(), 1).expect("append");

        let err = w.flush().expect_err("no sender is still an error");

        assert!(
            format!("{err:#}").contains("no ILP sender"),
            "unchanged message, got: {err:#}"
        );
        assert_eq!(w.pending(), 0, "the buffer was cleared by the rescue");
        assert_eq!(
            std::fs::read_dir(&dir)
                .expect("spill dir")
                .filter_map(std::result::Result::ok)
                .count(),
            1,
            "the synchronous rescue tier still runs for an unsplit writer"
        );
    }

    #[test]
    fn spill_failed_ilp_writes_the_payload_verbatim_so_it_can_be_replayed() {
        // The whole point of storing ILP text rather than a bespoke format:
        // the file IS a valid QuestDB /write body. If this ever stored a
        // re-encoded shape, the documented one-command recovery would be a
        // lie and nobody would find out until they needed it.
        let dir = scratch_dir("verbatim");
        let payload = b"ticks,feed=dhan,segment=IDX_I security_id=13i 1700000000000000000\n";
        let path = spill_failed_ilp(&dir, payload, Feed::Dhan, 1_700_000_000)
            .expect("spill writes to a fresh dir");
        let written = std::fs::read(&path).expect("spill file is readable");
        assert_eq!(
            written.as_slice(),
            payload,
            "the spill must be byte-identical to the ILP the flush would have sent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_failed_ilp_appends_rather_than_truncating_a_second_failure() {
        // Two failures in one hour land in the same file. Truncating would
        // silently discard the first episode's rows while reporting both as
        // rescued -- a false-OK inside the rescue itself.
        let dir = scratch_dir("append");
        let first = b"ticks first=1i 1700000000000000000\n";
        let second = b"ticks second=2i 1700000000000000001\n";
        let p1 = spill_failed_ilp(&dir, first, Feed::Dhan, 1_700_000_000).expect("first spill");
        let p2 = spill_failed_ilp(&dir, second, Feed::Dhan, 1_700_000_000).expect("second spill");
        assert_eq!(p1, p2, "same feed, same hour, same file");
        let written = std::fs::read(&p1).expect("readable");
        let mut expected = first.to_vec();
        expected.extend_from_slice(second);
        assert_eq!(written, expected, "both episodes survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_failed_ilp_separates_feeds_and_hours() {
        // Bounded file count with a replayable granularity: an operator
        // repairing a known-bad window must not have to re-ingest the day.
        let dir = scratch_dir("split");
        let a = spill_failed_ilp(&dir, b"a\n", Feed::Dhan, 1_700_000_000).expect("a");
        let b = spill_failed_ilp(&dir, b"b\n", Feed::Dhan, 1_700_003_600).expect("b");
        assert_ne!(a, b, "a different hour is a different file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_dir_bytes_sums_only_files_and_survives_an_unreadable_dir() {
        // Non-vacuity plus the fail-soft arm: failing to MEASURE the cap must
        // not fail the rescue the cap protects, so a missing dir reads 0
        // rather than propagating.
        let dir = scratch_dir("bytes");
        assert_eq!(
            spill_dir_bytes(&dir),
            0,
            "a dir that does not exist reads 0"
        );
        spill_failed_ilp(&dir, b"0123456789", Feed::Dhan, 1_700_000_000).expect("spill");
        assert_eq!(spill_dir_bytes(&dir), 10, "exactly the bytes written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Quarantined files count toward the spill ceiling.
    ///
    /// Before 2026-08-28 `spill_dir_bytes` was non-recursive and `quarantine/`
    /// is a subdirectory, so files the replay set aside as permanently refused
    /// counted toward NOTHING — not this ceiling and not any free-space check.
    /// They grew without any bound at all, on the same volume that filled
    /// completely on 2026-08-25 and WAL-suspended fifteen tables.
    ///
    /// They are not junk: a whole file is quarantined because ONE row is
    /// malformed, and a recorded case had 1,292 good rows out of 1,293. So this
    /// was real tick data silently consuming the disk.
    #[test]
    fn quarantined_bytes_count_toward_the_spill_ceiling() {
        let dir = scratch_dir("quarantine-counts");
        spill_failed_ilp(&dir, b"0123456789", Feed::Dhan, 1_700_000_000).expect("spill");
        assert_eq!(spill_dir_bytes(&dir), 10, "baseline: the live spill file");

        let q = dir.join(crate::tick_spill_replay::QUARANTINE_DIR);
        std::fs::create_dir_all(&q).expect("quarantine dir");
        std::fs::write(q.join("refused-0001.ilp"), b"abcdefghijklmno").expect("quarantined file");

        assert_eq!(
            spill_dir_bytes(&dir),
            25,
            "a quarantined file must count against the ceiling — uncounted, it \
             grows without bound and consumes the volume instead, which is what \
             took fifteen tables down on 2026-08-25"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The descent is exactly one level. A directory nested INSIDE quarantine is
    /// not walked, so this can never become an unbounded filesystem sweep on a
    /// path that runs before every rescue.
    #[test]
    fn the_spill_size_walk_descends_exactly_one_level() {
        let dir = scratch_dir("quarantine-depth");
        let q = dir.join(crate::tick_spill_replay::QUARANTINE_DIR);
        let deeper = q.join("nested");
        std::fs::create_dir_all(&deeper).expect("nested dir");
        std::fs::write(q.join("counted.ilp"), b"12345").expect("counted");
        std::fs::write(deeper.join("ignored.ilp"), b"9999999999").expect("ignored");

        assert_eq!(
            spill_dir_bytes(&dir),
            5,
            "only the quarantine directory itself is walked; going deeper would \
             make a pre-rescue sizing call an unbounded tree walk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_spill_ceiling_is_a_real_bound_that_scales_with_the_volume() {
        // A rescue that can fill the root volume is not a rescue: QuestDB and
        // the frame WAL share this disk. Past the ceiling the rows are dropped
        // and counted -- an honest failure, never a hidden one.
        //
        // But the ceiling must also be big enough to actually rescue. The
        // fixed 512 MiB it replaced was 0.26% of the 200 GB volume, and on
        // 2026-08-25 it refused a boot-time rescue and 1,695,983 ticks were
        // lost permanently in one event.
        assert!(
            TICK_SPILL_MIN_MAX_BYTES > 0,
            "the floor must be a real bound"
        );
        let ceiling = tick_spill_max_bytes();
        assert!(
            ceiling >= TICK_SPILL_MIN_MAX_BYTES,
            "the derived ceiling must never be SMALLER than the old fixed cap — \
             this change exists to stop losing ticks, not to tighten the bound"
        );
        // Resolved once: the enforcement can never disagree with a logged value.
        assert_eq!(ceiling, tick_spill_max_bytes(), "must be memoised");
        // And it must stay a small fraction, whatever the disk.
        assert!(
            TICK_SPILL_VOLUME_FRACTION >= 16,
            "the tier must never be able to threaten the database it rescues from"
        );
        assert_eq!(TICK_SPILL_DIR, "data/spill/ticks");
    }

    #[test]
    fn discard_pending_on_an_empty_buffer_writes_no_spill_file() {
        // Non-vacuity for the whole rescue: a healthy writer must never touch
        // the spill path. Without this, a rescue that fired unconditionally
        // would pass every test above while writing a file per flush.
        let mut writer = TickWriter::for_test(Feed::Dhan);
        assert_eq!(
            writer.discard_pending(),
            0,
            "nothing pending, nothing rescued"
        );
    }

    #[test]
    fn discard_pending_rescues_real_rows_to_the_spill_instead_of_dropping_them() {
        // The behaviour that actually matters, exercised through the REAL
        // path rather than the helper. 1,377 unrepeatable ticks were dropped
        // on 2026-08-21 by the code this replaces; this asserts they would
        // now be on disk and replayable.
        let dir = scratch_dir("rescue");
        let mut writer = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        writer
            .append_tick(&sample_tick())
            .expect("a tick buffers without a sender");
        assert_eq!(writer.pending(), 1, "one row is pending");

        let moved = writer.discard_pending();

        assert_eq!(moved, 1, "the row left the buffer");
        assert_eq!(writer.pending(), 0, "the buffer is cleared either way");
        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("the rescue created the dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(files.len(), 1, "exactly one spill file");
        let body = std::fs::read_to_string(files[0].path()).expect("readable");
        assert!(
            body.contains("ticks"),
            "the spill holds the ILP row, not an empty file: {body}"
        );
        assert!(
            !body.is_empty(),
            "a zero-byte rescue would report success while losing the rows"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- never-traded sentinel timestamp (2026-08-21) -----------------------

    /// Dhan's measured never-traded LTT sentinel: 1980-01-01, IST-epoch secs.
    const SENTINEL_LTT: u32 = 315_532_800;

    /// BITE TEST (2026-08-25) — the year-2106 partition.
    ///
    /// `exchange_timestamp` is a raw `u32` off the wire, so `0xFFFFFFFF` is
    /// ~2106-02-07. The stamp had a FLOOR only, so that value passed straight
    /// through and became the row's DESIGNATED timestamp — a far-future
    /// QuestDB partition that retention and archival, which key on the trading
    /// day, can never reach, while every `max(ts)` and range scan over `ticks`
    /// silently includes it.
    ///
    /// Deleting the `<= MAX_PLAUSIBLE_EXCHANGE_TS_SECS` half of the guard makes
    /// this fail.
    #[test]
    fn an_all_ones_exchange_timestamp_never_stamps_a_year_2106_partition() {
        let received = 1_787_000_000_i64 * 1_000_000_000;
        let stamped = row_timestamp_ist_nanos(u32::MAX, Some(received));
        assert_eq!(
            stamped, received,
            "an out-of-band LTT must fall back to the receipt time, exactly as \
             a below-floor one does"
        );
        // And the ceiling is a real edge, not a rounded-off approximation.
        let max = tickvault_trading::candles::multi_tf_aggregator::MAX_PLAUSIBLE_EXCHANGE_TS_SECS;
        assert_eq!(
            row_timestamp_ist_nanos(max, Some(received)),
            i64::from(max) * 1_000_000_000,
            "the last in-band second must still keep the exchange's own time"
        );
        assert_eq!(
            row_timestamp_ist_nanos(max + 1, Some(received)),
            received,
            "one second past the ceiling must fall back"
        );
    }

    /// BITE TEST (2026-08-25) — a NaN `average_traded_price` reaching ILP.
    ///
    /// The finiteness loop guards five fields; `average_traded_price` is a
    /// sixth caller of the same closure and was never in it. `NaN != 0.0` is
    /// true, so it passed the "not carried" gate and went to the wire — the
    /// exact batch-reject → spill → wedged-replay chain that
    /// `TickRowError::PriceNotFinite` documents as closed.
    #[test]
    fn a_non_finite_average_traded_price_becomes_null_and_never_refuses_the_tick() {
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: 100.0,
            exchange_timestamp: 1_787_000_000,
            average_traded_price: f32::NAN,
            ..Default::default()
        };
        let row = TickRow::from_parsed_tick(&tick, 1).expect("a good LTP must still produce a row"); // APPROVED: test
        assert_eq!(
            row.avg_price, None,
            "a non-finite optional price must be NULL, never NaN on the wire"
        );
        assert!(
            (row.ltp - 100.0).abs() < f64::EPSILON,
            "and the tick itself must survive — refusing it would lose a good \
             LTP to protect an auxiliary column"
        );
    }

    #[test]
    fn row_timestamp_ist_nanos_keeps_a_real_exchange_time_untouched() {
        // Non-vacuity, and the one that protects every NORMAL row: a traded
        // tick must keep the exchange's own time. If this ever returned the
        // receipt time, every price in the database would be stamped with when
        // WE saw it rather than when it TRADED -- silently, and for everything.
        let ltt: u32 = 1_787_000_000;
        assert_eq!(
            row_timestamp_ist_nanos(ltt, Some(9_999_999_999_999_999)),
            i64::from(ltt) * 1_000_000_000,
            "a plausible LTT wins over the receipt time, always"
        );
    }

    #[test]
    fn row_timestamp_ist_nanos_falls_back_for_the_1980_sentinel() {
        // The measured case: 945,501 rows on 2026-08-21 carried this exact
        // value, and 99.2% of them carried a real order book with it.
        let received = 1_787_300_000_000_000_000_i64;
        assert_eq!(
            row_timestamp_ist_nanos(SENTINEL_LTT, Some(received)),
            received,
            "a sentinel is not a time — the row takes its receipt time"
        );
    }

    #[test]
    fn row_timestamp_ist_nanos_falls_back_for_a_literal_zero_ltt() {
        // The other 7,282 rows. Both sentinel shapes must behave identically,
        // or one of them silently keeps landing in 1970.
        let received = 1_787_300_000_000_000_000_i64;
        assert_eq!(row_timestamp_ist_nanos(0, Some(received)), received);
    }

    #[test]
    fn row_timestamp_ist_nanos_keeps_the_sentinel_when_no_receipt_time_exists() {
        // Fail-safe. With no real time available, a GUESS would be worse than a
        // value that is visibly wrong: the row stays out of the live range and
        // stays findable as an anomaly, exactly as it does today.
        assert_eq!(
            row_timestamp_ist_nanos(SENTINEL_LTT, None),
            315_532_800_000_000_000
        );
        assert_eq!(row_timestamp_ist_nanos(0, None), 0);
    }

    #[test]
    fn row_timestamp_ist_nanos_agrees_with_the_aggregators_own_floor() {
        // The floor is SHARED with the aggregator rather than re-declared, so
        // the candle refusal and the timestamp fallback can never disagree
        // about what counts as a real time. Pinned at the exact boundary.
        let floor = tickvault_trading::candles::multi_tf_aggregator::MIN_PLAUSIBLE_EXCHANGE_TS_SECS;
        let received = 1_787_300_000_000_000_000_i64;
        assert_eq!(
            row_timestamp_ist_nanos(floor, Some(received)),
            i64::from(floor) * 1_000_000_000,
            "exactly AT the floor is plausible and must be kept"
        );
        assert_eq!(
            row_timestamp_ist_nanos(floor - 1, Some(received)),
            received,
            "one second below the floor is a sentinel"
        );
    }

    #[test]
    fn a_sentinel_row_keeps_its_raw_ltt_and_its_order_book() {
        // The whole reason these rows are kept rather than dropped: they carry
        // a live book. This drives the REAL row builder and asserts the row is
        // now findable in the live time range while losing NOTHING -- the raw
        // sentinel survives in its own column, so "never traded" is still
        // recoverable.
        let mut tick = sample_tick();
        tick.exchange_timestamp = SENTINEL_LTT;
        tick.last_traded_price = 0.0;
        tick.total_buy_quantity = 8_397_000;
        tick.total_sell_quantity = 9_019_000;
        tick.received_at_nanos = 1_787_300_000_000_000_000;

        let row = TickRow::from_parsed_tick(&tick, 1).expect("a sentinel tick still builds a row");

        assert_ne!(
            row.ts_ist_nanos,
            i64::from(SENTINEL_LTT) * 1_000_000_000,
            "the row must no longer be stamped into the 1980 partition"
        );
        assert_eq!(
            row.exchange_timestamp,
            Some(i64::from(SENTINEL_LTT)),
            "the raw sentinel is preserved — nothing is destroyed"
        );
        assert_eq!(row.total_buy_qty, Some(8_397_000), "the book survives");
        assert_eq!(row.total_sell_qty, Some(9_019_000), "the book survives");
    }

    #[test]
    fn the_rescue_path_still_increments_the_alarmed_counter() {
        // REGRESSION RATCHET (2026-08-21). The first version of the spill
        // rescue incremented ONLY `tv_ticks_spilled_total`, which is neither
        // EMF-selected nor alarmed, and thereby diverted the common flush
        // failure off `dhan_ticks_dropped` — the only pager watching it. The
        // rescue made the loss recoverable and simultaneously made it
        // INVISIBLE, which is a strictly worse trade and the exact false-OK
        // this repo forbids.
        //
        // Source-scan rather than a recorder assertion, deliberately: what
        // must never regress is that the ALARMED NAME appears on the success
        // arm. A behavioural test on a metrics recorder would still pass if
        // someone renamed the counter to another unalarmed one.
        let src = include_str!("tick_persistence.rs");
        let ok_arm = src
            .split("Ok(path) => {")
            .nth(1)
            .expect("the spill-success arm exists");
        let arm_body = ok_arm
            .split("Err(err) => {")
            .next()
            .expect("bounded by the failure arm");
        assert!(
            arm_body.contains("tv_ticks_dropped_total"),
            "the ALARMED counter must fire on a rescue too — a rescued row is \
             still absent from QuestDB, which is what dhan_ticks_dropped pages \
             on. Removing it silently un-pages the flush failure."
        );
        assert!(
            arm_body.contains("tv_ticks_spilled_total"),
            "the recoverable-subset counter must also fire, or the operator \
             cannot tell a rescued loss from a permanent one"
        );
    }

    /// The ratchet this closes: quarantine grows, counts toward the ceiling,
    /// and nothing ever removed from it — so it could permanently disable the
    /// rescue tier for every future boot.
    #[test]
    fn quarantine_is_trimmed_to_its_share_of_the_spill_ceiling() {
        let dir = std::env::temp_dir().join(format!(
            "tv-quarantine-prune-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let q = dir.join(crate::tick_spill_replay::QUARANTINE_DIR);
        std::fs::create_dir_all(&q).expect("temp quarantine");

        // Budget = ceiling / 4. With a 400-byte ceiling that is 100 bytes.
        let ceiling = 400u64;
        for (i, name) in ["a.ilp", "b.ilp", "c.ilp", "d.ilp"].iter().enumerate() {
            std::fs::write(q.join(name), vec![b'x'; 60]).expect("write");
            // Distinct mtimes so "oldest first" is well-defined rather than
            // filesystem-order luck — a prune that deletes in an arbitrary
            // order would pass a byte-only assertion while destroying the
            // newest file, which is the one an investigation is about.
            let t = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000 + i as u64 * 60);
            let f = std::fs::File::options()
                .write(true)
                .open(q.join(name))
                .expect("reopen");
            f.set_modified(t).expect("set mtime");
        }

        let removed = prune_quarantine(&dir, ceiling);

        assert!(
            removed >= 3,
            "240 bytes must be trimmed to 100, got {removed} removed"
        );
        assert!(
            !q.join("a.ilp").exists(),
            "the OLDEST file must go first — a quarantined file's recoverability decays, \
             and the newest is the one an active investigation is about"
        );
        assert!(
            q.join("d.ilp").exists(),
            "the NEWEST quarantined file must survive the trim"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_quarantine_inside_its_budget_is_never_touched() {
        // The never-delete promise still holds for everything that fits. A
        // prune that trimmed unconditionally would destroy recoverable lines
        // for no reason — on 2026-08-25, 1,292 of 1,293 lines in a quarantined
        // file were recoverable by hand.
        let dir = std::env::temp_dir().join(format!(
            "tv-quarantine-keep-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let q = dir.join(crate::tick_spill_replay::QUARANTINE_DIR);
        std::fs::create_dir_all(&q).expect("temp quarantine");
        std::fs::write(q.join("small.ilp"), b"tiny").expect("write");

        assert_eq!(prune_quarantine(&dir, 4096), 0);
        assert!(q.join("small.ilp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_quarantine_directory_is_not_an_error() {
        // Boot housekeeping must never fail a boot. Most sessions have no
        // quarantine at all.
        let dir = std::env::temp_dir().join(format!(
            "tv-quarantine-absent-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        assert_eq!(prune_quarantine(&dir, 4096), 0);
    }

    #[test]
    fn quarantine_can_never_claim_the_majority_of_the_spill_budget() {
        // Quarantine holds files QuestDB has ALREADY refused; the rest of the
        // budget holds rows it would still accept. Letting the refused half
        // win would trade recoverable-by-hand bytes for rows lost outright.
        assert!(
            QUARANTINE_BUDGET_FRACTION >= 2,
            "a fraction below 2 lets quarantine take at least half the spill ceiling, \
             starving the LIVE rescue path that keeps rows QuestDB would still accept"
        );
    }

    /// Both `tier` label values must be seeded, not just one.
    ///
    /// The CloudWatch agent computes its delta PER LABEL SET, so seeding one
    /// tier leaves the other exactly as blind as before — a partially-seeded
    /// family is a partial blind spot wearing the appearance of a covered one.
    /// This is the failure this repository already paid for on the depth loss
    /// counters, which is why it is pinned rather than assumed.
    #[test]
    fn test_seed_spill_free_probe_blind_counters_seeds_both_tiers() {
        // Runs clean with no recorder installed, and twice — boot may call it
        // on a re-spawn path and a panic there would take the lane down for a
        // metric.
        seed_spill_free_probe_blind_counters();
        seed_spill_free_probe_blind_counters();

        // The property that actually matters is WHICH label values are seeded,
        // and that cannot be read back from the metrics facade without a
        // recorder. Pin it at the source instead.
        let src = include_str!("tick_persistence.rs");
        let body = src
            // Needle assembled from parts on PURPOSE: spelled as one literal it
            // reads as a `pub fn` DECLARATION to `pub-fn-test-guard.sh`, which
            // greps source, and this test would then count itself as a second
            // untested pub fn. Found by the guard blocking the push.
            .split_once(concat!(
                "pub ",
                "fn ",
                "seed_spill_free_probe_blind_counters()"
            ))
            .map(|(_, rest)| rest.split_once("\n}").map_or(rest, |(b, _)| b))
            .unwrap_or_default();
        for tier in ["\"tick\"", "\"depth\""] {
            assert!(
                body.contains(tier),
                "the seeding function does not register the {tier} tier — the \
                 agent seeds per LABEL SET, so that tier's first blind write \
                 would be eaten as its baseline and never seen"
            );
        }
        assert_eq!(
            body.matches("increment(0)").count(),
            2,
            "expected exactly two seeded label sets"
        );
    }

    #[test]
    fn an_out_of_order_rescue_never_advances_the_watermark() {
        // The global is shared across the test binary; use ranges far above
        // anything another test acks so the assertion is about THIS call.
        let wm = crate::wal_applied_watermark::applied_watermark();
        let base = 1u64 << 62;
        let before = wm.snapshot().hwm_ticks;
        note_rescue_outcome_ticks(true, (base, base + 10), false);
        assert_eq!(
            wm.snapshot().hwm_ticks,
            before,
            "a producer-side or rescue-thread landing acks nothing"
        );
        let unlanded = wm.unlanded_total();
        note_rescue_outcome_ticks(false, (base + 20, base + 30), false);
        assert_eq!(wm.unlanded_total(), unlanded + 1);
        assert!(wm.snapshot().range_has_unapplied(base + 25, base + 25));
    }
}
