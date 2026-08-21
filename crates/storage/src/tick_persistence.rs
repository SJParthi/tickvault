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
        let opt_price = |v: f32| (v != 0.0).then(|| round_to_2dp(f32_to_f64_clean(v)));
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

/// Hard ceiling on the spill directory, in bytes (512 MiB).
///
/// A rescue that can fill the root volume is not a rescue — QuestDB and the
/// frame WAL share this disk, so an unbounded spill would trade a bounded tick
/// loss for an unbounded outage of everything. Past the cap the rows ARE
/// dropped and counted, which is the same honest failure as today rather than
/// a worse one.
pub const TICK_SPILL_MAX_BYTES: u64 = 512 * 1024 * 1024;

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

    if spill_dir_bytes(dir) >= TICK_SPILL_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            format!("tick spill dir at or past its {TICK_SPILL_MAX_BYTES}-byte cap"),
        ));
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

/// Total bytes currently held in the spill directory.
///
/// Non-recursive and best-effort: an unreadable entry contributes 0 rather
/// than aborting the sweep, because failing to measure the cap must not also
/// fail the rescue the cap is protecting.
fn spill_dir_bytes(dir: &Path) -> u64 {
    // O(1) EXEMPT: begin — cold path, bounded by the per-feed-per-hour file count.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|m| m.len())
        .sum()
    // O(1) EXEMPT: end
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
#[must_use]
pub fn row_timestamp_ist_nanos(exchange_timestamp: u32, received_at_ist_nanos: Option<i64>) -> i64 {
    let ltt_nanos = i64::from(exchange_timestamp).saturating_mul(1_000_000_000);
    if exchange_timestamp
        >= tickvault_trading::candles::multi_tf_aggregator::MIN_PLAUSIBLE_EXCHANGE_TS_SECS
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
                    feed,
                    last_capture_seq: 0,
                    spill_dir: PathBuf::from(TICK_SPILL_DIR),
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
                    spill_dir: PathBuf::from(TICK_SPILL_DIR),
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
            feed,
            last_capture_seq: 0,
            spill_dir: PathBuf::from(TICK_SPILL_DIR),
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
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            let payload_len = self.buffer.as_bytes().len();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0_i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
            match spill_failed_ilp(&self.spill_dir, self.buffer.as_bytes(), self.feed, now) {
                Ok(path) => {
                    // BOTH counters, and the alarmed one is not optional.
                    //
                    // `tv_ticks_dropped_total` is EMF-selected and carries the
                    // `dhan_ticks_dropped` alarm (`live-lane-alarms.tf`).
                    // `tv_ticks_spilled_total` carries NEITHER. Incrementing
                    // only the new name would have DIVERTED the common flush
                    // failure — the exact 2026-08-21 timeout — off the only
                    // pager that watches it, so the operator would have been
                    // told less than before the rescue existed. That is a
                    // false-OK (audit Rule 11), and a rescue that blinds the
                    // alarm is a worse outcome than the loss it prevents.
                    //
                    // The alarmed counter is also the SEMANTICALLY correct one
                    // here: it means "rows left the buffer without reaching
                    // QuestDB", which is TRUE of a rescued row — the file is on
                    // disk, the database does not have it. `spilled` is the
                    // strictly narrower fact "and it is recoverable", which is
                    // why it is a second increment rather than a replacement.
                    metrics::counter!(
                        "tv_ticks_dropped_total",
                        "feed" => self.feed.as_str()
                    )
                    .increment(dropped as u64);
                    metrics::counter!(
                        "tv_ticks_spilled_total",
                        "feed" => self.feed.as_str()
                    )
                    .increment(dropped as u64);
                    error!(
                        code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                        feed = self.feed.as_str(),
                        rescued = dropped,
                        bytes = payload_len,
                        path = %path.display(),
                        "tick flush failed — the buffered rows were RESCUED to the tick \
                         spill file named here, not lost. They are NOT in QuestDB yet. \
                         Re-ingest is one command and is safe to repeat, because the \
                         ticks dedup key carries capture_seq: \
                         curl --data-binary @<path> http://<questdb>:9000/write"
                    );
                }
                Err(err) => {
                    // The rescue itself failed (disk full, cap reached, no
                    // permission). Fall back to the counted drop — a spill that
                    // cannot be written must never mask the loss.
                    metrics::counter!(
                        "tv_ticks_dropped_total",
                        "feed" => self.feed.as_str()
                    )
                    .increment(dropped as u64);
                    error!(
                        code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                        feed = self.feed.as_str(),
                        dropped,
                        spill_error = %err,
                        "tick flush failed AND the spill rescue also failed — these ticks \
                         are permanently lost and nothing re-inserts them. The raw frames \
                         remain in the write-ahead log for manual recovery."
                    );
                }
            }
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

    #[test]
    fn the_spill_cap_is_a_real_ceiling_not_a_suggestion() {
        // A rescue that can fill the root volume is not a rescue: QuestDB and
        // the frame WAL share this disk. Past the cap the rows are dropped and
        // counted -- the same honest failure as before, never a worse one.
        assert!(
            TICK_SPILL_MAX_BYTES > 0 && TICK_SPILL_MAX_BYTES <= 1024 * 1024 * 1024,
            "the cap must be a real bound well under the 200 GB volume"
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
}
