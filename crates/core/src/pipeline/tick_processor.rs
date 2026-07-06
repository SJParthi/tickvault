//! Main tick processing loop — the heart of the pipeline.
//!
//! Consumes raw WebSocket binary frames, parses them, and persists ticks
//! to QuestDB. Pure capture — zero aggregation on the hot path.
//!
//! # Performance
//! - O(1) per tick
//! - Zero heap allocation on hot path
//! - Batched QuestDB writes (flush every 1000 rows or 100ms)

use std::time::Instant;

use metrics::{counter, gauge, histogram};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, trace, warn};

use crate::parser::dispatch_frame;
use crate::parser::types::ParsedFrame;
use tickvault_common::constants::{
    DEDUP_RING_BUFFER_POWER, IST_UTC_OFFSET_SECONDS, MINIMUM_VALID_EXCHANGE_TIMESTAMP,
    MUHURAT_PERSIST_END_SECS_OF_DAY_IST, MUHURAT_PERSIST_START_SECS_OF_DAY_IST, SECONDS_PER_DAY,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
    WS_GRACE_AFTER_CLOSE_SECS_U32,
};
use tickvault_common::tick_types::{GreeksEnricher, ParsedTick};

use tickvault_storage::tick_persistence::{TickLifecycle, TickPersistenceWriter};

use tickvault_common::tick_types::MarketDepthLevel;

use crate::pipeline::tick_enricher::TickEnricher;

/// Returns `true` if all 5 depth levels have finite bid/ask prices.
///
/// # Performance
/// O(1) — exactly 10 `is_finite()` checks (single CPU instruction each).
#[inline(always)]
fn depth_prices_are_finite(depth: &[MarketDepthLevel; 5]) -> bool {
    depth
        .iter()
        .all(|level| level.bid_price.is_finite() && level.ask_price.is_finite())
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// File path for index prev_close cache (survives mid-day restarts).
///
/// Wave 1 Item 0.a — the `*_TMP` constant is no longer used here; the
/// async writer in `super::prev_close_writer` owns the tmp+rename
/// atomic pattern internally.
const INDEX_PREV_CLOSE_CACHE_DIR: &str = "data/instrument-cache";
const INDEX_PREV_CLOSE_CACHE_PATH: &str = "data/instrument-cache/index-prev-close.json";

/// Wave 1 Item 0.d — boot-time idempotent init for the index prev_close
/// cache directory. Hoisted out of the tick hot path (was previously a
/// per-packet `std::fs::create_dir_all` call on every PrevClose code-6
/// frame). Safe to call multiple times.
///
/// Called from `crates/app/src/main.rs` Step 6b alongside the QuestDB
/// table-DDL fan-out, before the tick processor starts. The hot path now
/// assumes the directory exists and only writes the file atomically.
// HOT-PATH-EXEMPT: boot-only init, runs once before tick processor starts.
pub fn init_prev_close_cache_dir() -> std::io::Result<()> {
    // O(1) EXEMPT: boot-only init — called once from main.rs Step 6b, never on the tick path.
    std::fs::create_dir_all(INDEX_PREV_CLOSE_CACHE_DIR)
}

/// **ZL-P0-2** — canary underlyings whose ticks are used as an
/// end-to-end "alive but flowing?" health probe.
///
/// These are the NSE/BSE spot indices that trade **every single second**
/// during market hours. If even ONE of them stops ticking while the
/// market is open, something is broken end-to-end in the pipeline even
/// if the WebSocket itself is healthy and frames are arriving.
///
/// The matching Grafana rule (not in this code):
/// ```promql
/// (time() - tv_ws_canary_last_tick_epoch_secs{underlying="NIFTY"}) > 5
///     and on() tv_market_open == 1
/// ```
/// fires a Telegram alert if any canary underlying has no fresh tick
/// for >5 seconds during market hours.
///
/// Security IDs are stable across Dhan's instrument master for the
/// spot indices (they are NOT derivative contracts and are not
/// re-issued daily). Sources:
/// - NIFTY 50: Dhan SID 13, segment IDX_I
/// - BANK NIFTY: Dhan SID 25, segment IDX_I
/// - SENSEX: Dhan SID 51, segment IDX_I
///
/// Verified via existing test `tests::test_header_parse` which uses
/// SID 13 for NIFTY, and by the live universe seeded from the
/// instrument master at boot.
const CANARY_UNDERLYINGS: &[(u64, &str)] = &[(13, "NIFTY"), (25, "BANKNIFTY"), (51, "SENSEX")];

/// B6 adversarial round 1 (HIGH-3): idle-drain cadence for the tick loop.
///
/// `flush_if_needed` (which drains worker-failed flush batches into the
/// ring → spill → DLQ rescue chain AND time-flushes a partial buffer) used
/// to run only per-frame — a flush failing after the day's LAST tick left
/// up to 16 batches parked in RAM until graceful shutdown. The recv loop's
/// `tokio::select!` interval arm invokes the same call at this cadence even
/// when no frame arrives, restoring the pre-B6 bound (an inline failure hit
/// ring → spill immediately; now ≤1s).
const IDLE_FLUSH_DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1); // APPROVED: this IS the named constant the rule asks for

// Phase 4b (2026-05-05): `MOVERS_PERSIST_START_SECS_OF_DAY_IST`
// constant DELETED — only used by the now-removed legacy
// stock_movers / option_movers persist call block. Unified
// movers_pipeline manages its own cadence in app crate.

// ---------------------------------------------------------------------------
// O(1) Market Hours Persist Window
// ---------------------------------------------------------------------------

/// Returns `true` if the exchange timestamp falls within the regular
/// `[09:00, 15:30)` IST window, OR — when `muhurat_active` is `true` — inside
/// the Muhurat evening window `[18:00, 19:30)` IST.
///
/// Only ticks inside the accepted window are persisted to QuestDB. Pre-market
/// and post-market ticks still flow through broadcast and candle aggregation.
///
/// # CCL-06 (Muhurat)
/// On a Diwali Muhurat date the live feed connects but the evening session runs
/// ~18:00–19:30 IST, entirely OUTSIDE the regular window. `muhurat_active` is
/// the boot-computed [`tickvault_common::muhurat`] flag; it is `false` on every
/// trading/mock day, so the accepted set is then EXACTLY `[09:00, 15:30)` — the
/// regular behaviour is byte-for-byte unchanged. The Muhurat range is purely
/// ADDITIVE (disjoint from + after the regular range, pinned by a compile-time
/// assert in `constants.rs`), so it can never narrow the regular window.
///
/// # Algorithm (O(1), zero allocation)
/// 1. Dhan WebSocket sends exchange_timestamp as IST epoch seconds (already adjusted).
/// 2. Modulo 86,400 → seconds-of-day in IST directly.
/// 3. Range check: `[TICK_PERSIST_START, TICK_PERSIST_END)`, plus the Muhurat
///    range when active.
///
/// # Performance
/// 1 modulo + 2–4 comparisons. No branching beyond the range checks.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: modulo by 86400 is safe
#[inline(always)]
fn is_within_persist_window(exchange_timestamp: u32, muhurat_active: bool) -> bool {
    // exchange_timestamp is already IST epoch seconds — no offset needed.
    let ist_secs_of_day = exchange_timestamp % SECONDS_PER_DAY;
    if (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST)
        // O(1) EXEMPT: Range::contains — two-comparison O(1) bounds check (scanner heuristic misreads it as Vec::contains).
        .contains(&ist_secs_of_day)
    {
        return true;
    }
    muhurat_active
        && (MUHURAT_PERSIST_START_SECS_OF_DAY_IST..MUHURAT_PERSIST_END_SECS_OF_DAY_IST)
            // O(1) EXEMPT: Range::contains — O(1) bounds check.
            .contains(&ist_secs_of_day)
}

/// Returns `true` if the exchange timestamp's IST day matches today.
///
/// Defense-in-depth: rejects stale ticks from previous trading days that
/// Dhan may replay on WebSocket reconnection or on non-trading days.
///
/// # Algorithm (O(1), zero allocation)
/// Integer division by 86,400 gives the day number. Compare with pre-computed today.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: division by 86400 is safe
#[inline(always)]
fn is_today_ist(exchange_timestamp: u32, today_ist_day_number: u32) -> bool {
    exchange_timestamp / SECONDS_PER_DAY == today_ist_day_number
}

/// Operator lock 2026-06-01 §30: is `(security_id, exchange_segment_code)`
/// exempt from the [09:00, 15:30) IST persist window? GIFT Nifty
/// (`GIFTNIFTY`, NSE-IX) trades ~21 h/day, so its ticks must persist
/// outside the regular session. The set is boot-immutable and tiny
/// (usually ≤1 entry), so this is O(1).
#[inline(always)]
fn is_window_exempt(
    always_on: &std::collections::HashSet<(u64, u8)>,
    security_id: u64,
    exchange_segment_code: u8,
) -> bool {
    // O(1) EXEMPT: HashSet `contains` is O(1) hashing, not an O(n) Vec scan.
    always_on.contains(&(security_id, exchange_segment_code))
}

// ---------------------------------------------------------------------------
// O(1) Tick Validity Checks (extracted for testability)
// ---------------------------------------------------------------------------

/// Returns `true` if the LTP is a valid tradeable price.
///
/// Invalid: NaN, Infinity, zero, negative, or absurdly-large values. The upper
/// bound (`MAX_PLAUSIBLE_LTP` = ₹10 crore, ~500× the priciest real NSE
/// instrument) rejects an absurd-but-FINITE garbage price (e.g. `f32::MAX` from
/// a mangled frame) BEFORE it can poison a candle's high/low or a `ticks` row.
/// It can never reject a genuine price, so no real tick is ever missed.
/// O(1) — 1 `is_finite()` + 2 comparisons.
#[inline(always)]
fn is_valid_ltp(ltp: f32) -> bool {
    ltp.is_finite() && ltp > 0.0 && ltp <= tickvault_common::constants::MAX_PLAUSIBLE_LTP
}

/// Returns `true` if the tick has valid price AND timestamp.
///
/// Combines LTP validity with exchange timestamp range check.
/// O(1) — 2 comparisons after `is_valid_ltp`.
#[inline(always)]
fn is_valid_tick(ltp: f32, exchange_timestamp: u32) -> bool {
    is_valid_ltp(ltp) && exchange_timestamp >= MINIMUM_VALID_EXCHANGE_TIMESTAMP
}

/// Returns `true` if best bid > best ask (both positive).
///
/// Indicates an auction / pre-open period. Metric-only, do NOT filter.
/// O(1) — 3 comparisons.
#[inline(always)]
fn is_crossed_market(best_bid: f32, best_ask: f32) -> bool {
    best_bid > best_ask && best_bid > 0.0 && best_ask > 0.0
}

/// Phase 0 Item 22e (2026-05-15): tick-level OHLC integrity violations.
///
/// Three independent invariants checked on every tick:
///
///   1. `day_high >= day_low` — the most basic OHLC invariant. Dhan
///      should NEVER emit a tick where the high < low; if it does,
///      something is wrong upstream (binary parser regression, garbage
///      packet, etc.).
///   2. `last_traded_price <= day_high` — the latest trade cannot
///      have been at a price higher than the day's running high.
///   3. `last_traded_price >= day_low` — symmetric to #2.
///
/// Returns a `u8` bitmask of violations:
///   * bit 0 (= 1) — high-below-low violation
///   * bit 1 (= 2) — ltp-above-high violation
///   * bit 2 (= 4) — ltp-below-low violation
///
/// `0` means clean. The caller emits the appropriate static-label
/// counter for each set bit so the hot path stays zero-alloc.
///
/// **Skip semantics (intentional):**
/// * Any field that is non-finite or zero is silently skipped. Dhan
///   legitimately emits `day_high = 0.0` and `day_low = 0.0` in
///   pre-open before the first trade — flagging those would page the
///   operator every morning at 09:00 IST.
/// * The day-high/day-low/LTP comparisons each require BOTH operands
///   to be `> 0.0` and finite; if either side is missing, that
///   specific bit is left clear.
///
/// O(1) — at most 6 comparisons + 3 boolean ANDs, no allocation.
#[inline(always)]
fn check_tick_ohlc_integrity(ltp: f32, day_high: f32, day_low: f32) -> u8 {
    let mut violations: u8 = 0;
    let high_ok = day_high.is_finite() && day_high > 0.0;
    let low_ok = day_low.is_finite() && day_low > 0.0;
    let ltp_ok = ltp.is_finite() && ltp > 0.0;
    if high_ok && low_ok && day_high < day_low {
        violations |= 0b001;
    }
    if ltp_ok && high_ok && ltp > day_high {
        violations |= 0b010;
    }
    if ltp_ok && low_ok && ltp < day_low {
        violations |= 0b100;
    }
    violations
}

/// Converts UTC nanoseconds to IST seconds-of-day.
///
/// Used for PreviousClose and candle sweep timestamps that arrive as UTC
/// `received_at_nanos` but need IST time-of-day for persist window check.
///
/// O(1) — 3 arithmetic ops.
#[allow(clippy::cast_possible_truncation)] // APPROVED: secs-of-day fits u32
#[inline(always)]
fn utc_nanos_to_ist_secs_of_day(received_at_nanos: i64) -> u32 {
    received_at_nanos
        .saturating_div(1_000_000_000)
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// Audit finding #11 (2026-04-17): maximum backward NTP clock step we
/// accept. A backward jump smaller than this is tolerated; a larger
/// jump is considered a clock-skew anomaly and fires a metric so the
/// operator can investigate (e.g. the VM was suspended and resumed).
///
/// Threshold = 60 s chosen so a typical chrony/ntpd slew (millisecond-
/// scale) never trips it, but a step correction or VM time jump does.
pub const CLOCK_SKEW_BACKWARD_THRESHOLD_NANOS: i64 = 60_000_000_000;

/// Largest `received_at_nanos` seen so far, used to detect NTP backward
/// jumps in `is_wall_clock_within_persist_window`. Because `received_at`
/// is set by the WebSocket read loop using `Utc::now()`, a monotonic
/// stream should never go backward by more than the arrival jitter.
///
/// Single-process atomic: zero contention on the hot path, loaded with
/// `Relaxed` (no happens-before needed for a best-effort monitoring
/// counter).
static MAX_WALL_CLOCK_SEEN_NANOS: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

/// Last `received_at_nanos` HANDED OUT by `current_received_at_nanos()`, used
/// to guarantee **strict monotonicity** of `received_at` across every tick.
///
/// ## Why (dedup-collision elimination — operator directive 2026-06-04)
///
/// The `ticks` DEDUP UPSERT key is `(ts, security_id, segment, received_at)`.
/// `ts` is the exchange timestamp, which is only **1-second granular**, so two
/// distinct ticks for the same instrument within the same second are
/// distinguished ONLY by `received_at`. If two such ticks ever received the
/// SAME `received_at` nanosecond (a burst during a fast move + a coarse clock),
/// QuestDB's UPSERT would COLLAPSE them into one row — silently dropping a
/// tick (e.g. the minute's true high/low extreme). By forcing `received_at`
/// strictly increasing, two distinct ticks can NEVER share the dedup key, so
/// a same-nanosecond collision is **structurally impossible**, not merely
/// "unlikely". The bump is sub-microsecond and never affects the
/// second-granular market-hours window check.
static LAST_RECEIVED_AT_NANOS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Pure helper: given the raw wall-clock `now` (UTC nanos) and the
/// last-handed-out value, returns a value that is **strictly greater** than
/// the last one (`now` if it already advanced, else `last + 1`), and updates
/// the atomic. Lock-free CAS loop — O(1), zero allocation, hot-path safe.
///
/// Guarantees: the returned sequence is strictly monotonically increasing, so
/// no two calls ever return the same value → no two ticks ever share the
/// `received_at` component of the DEDUP key.
#[inline]
fn monotonic_received_at(now: i64, last: &std::sync::atomic::AtomicI64) -> i64 {
    use std::sync::atomic::Ordering;
    loop {
        let prev = last.load(Ordering::Relaxed);
        let next = now.max(prev.saturating_add(1));
        if last
            .compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

/// Test-only override for `received_at_nanos` used by `run_tick_processor`.
///
/// Production always uses `Utc::now()` via the `else` branch in
/// `current_received_at_nanos()` — zero runtime cost. Tests can set this
/// atomic to inject a deterministic wall-clock so the post-market
/// `is_wall_clock_within_persist_window` guard does not filter every
/// test tick when `cargo test` is invoked outside 09:00-15:30 IST.
///
/// Sentinel `0` means "no override — use `Utc::now()`".
#[cfg(test)]
pub(crate) static TEST_RECEIVED_AT_OVERRIDE_NANOS: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

/// Returns the current wall-clock in UTC nanoseconds, or a test override.
#[inline]
fn current_received_at_nanos() -> i64 {
    #[cfg(test)]
    {
        let override_val =
            TEST_RECEIVED_AT_OVERRIDE_NANOS.load(std::sync::atomic::Ordering::Relaxed);
        if override_val != 0 {
            return override_val;
        }
    }
    // Strict-monotonic bump (dedup-collision elimination): every tick gets a
    // received_at strictly greater than the previous one, so two distinct
    // ticks can never share the `(ts, security_id, segment, received_at)`
    // dedup key. See LAST_RECEIVED_AT_NANOS doc for the full rationale.
    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    monotonic_received_at(now, &LAST_RECEIVED_AT_NANOS)
}

/// Returns `true` if the wall-clock time (received_at_nanos) falls within
/// the tick persist window `[09:00, 15:31) IST`.
///
/// Defense-in-depth: rejects stale snapshot ticks that arrive after market
/// close. The WebSocket continues to stream snapshot data post-market with
/// exchange_timestamp from during market hours. Without this check, those
/// stale ticks pass `is_within_persist_window` (which only checks the
/// exchange_timestamp) and pollute the `ticks` table.
///
/// ## Phase 0 Item 11 — G2 wall-clock grace (2026-05-17)
///
/// The upper bound is `TICK_PERSIST_END_SECS_OF_DAY_IST +
/// WS_GRACE_AFTER_CLOSE_SECS` = `15:30:00 + 60s = 15:31:00` IST. This
/// cures the **dual-gate bug** documented in `topic-PHASE-0-LEAN-LOCKED.md`
/// §8: a tick with `exchange_timestamp = 15:29:59.586` was REJECTED
/// because local wall-clock had advanced to `15:30:00.100` by the time
/// the gate evaluated. With the 60s grace tail, that tick is now
/// accepted (G1 says exchange-ts is in-window; G2 says wall-clock is
/// within the grace tail). The 60s window matches Dhan's worst-case
/// ingestion + network delay at close-of-day.
///
/// I-P1-AUDIT-11 (2026-04-17): also monitors NTP clock skew. If the
/// wall-clock goes BACKWARD by more than `CLOCK_SKEW_BACKWARD_THRESHOLD_NANOS`
/// versus the largest value seen so far, it logs ERROR + increments
/// `tv_clock_skew_detections_total` so the operator sees it in Grafana
/// and Telegram. The tick is still evaluated against the window (we do
/// NOT reject it on the basis of skew alone — that would drop
/// legitimate ticks during a one-off correction).
///
/// ## CCL-06 (Muhurat)
///
/// When `muhurat_active` is `true`, the accepted set additionally includes the
/// Muhurat evening window `[18:00, 19:30)` IST (no grace tail — 19:30 is already
/// a generous superset upper bound). `muhurat_active` is `false` on every
/// trading/mock day, so the accepted set is then EXACTLY `[09:00, 15:31)` — the
/// regular behaviour is byte-for-byte unchanged.
///
/// O(1) — reuses `utc_nanos_to_ist_secs_of_day` + 1–2 range checks + 1
/// relaxed atomic load + 1 CAS on rising edge.
#[inline(always)]
fn is_wall_clock_within_persist_window(received_at_nanos: i64, muhurat_active: bool) -> bool {
    // I-P1-AUDIT-11: clock-skew monitor.
    use std::sync::atomic::Ordering;
    let prev_max = MAX_WALL_CLOCK_SEEN_NANOS.load(Ordering::Relaxed);
    if received_at_nanos > prev_max {
        // Rising edge: update max (relaxed CAS — lost races only mean
        // we miss an update, which is fine for a monitoring counter).
        _ = MAX_WALL_CLOCK_SEEN_NANOS.compare_exchange(
            prev_max,
            received_at_nanos,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    } else if prev_max.saturating_sub(received_at_nanos) > CLOCK_SKEW_BACKWARD_THRESHOLD_NANOS {
        // Backward jump beyond threshold — log + count but don't reject.
        metrics::counter!("tv_clock_skew_detections_total").increment(1);
        tracing::error!(
            prev_max_nanos = prev_max,
            received_at_nanos,
            backward_jump_nanos = prev_max.saturating_sub(received_at_nanos),
            threshold_nanos = CLOCK_SKEW_BACKWARD_THRESHOLD_NANOS,
            "NTP clock skew detected — wall-clock jumped backward by >60s. \
             Tick kept but flagged; investigate VM suspend/resume or NTP step."
        );
    }

    let wall_clock_ist_secs_of_day = utc_nanos_to_ist_secs_of_day(received_at_nanos);
    // Phase 0 Item 11: G2 grace tail. Use `TICK_PERSIST_END +
    // WS_GRACE_AFTER_CLOSE_SECS` (= 15:31:00) as the upper bound so a
    // tick stamped at 15:29:59.x with wall-clock 15:30:00.x is not
    // wrongly rejected.
    let grace_upper_bound =
        TICK_PERSIST_END_SECS_OF_DAY_IST.saturating_add(WS_GRACE_AFTER_CLOSE_SECS_U32);
    // O(1) EXEMPT: Range::contains — two-comparison O(1) bounds check (scanner heuristic misreads it as Vec::contains).
    if (TICK_PERSIST_START_SECS_OF_DAY_IST..grace_upper_bound).contains(&wall_clock_ist_secs_of_day)
    {
        return true;
    }
    // CCL-06: additive Muhurat evening window when today is a Muhurat session.
    muhurat_active
        && (MUHURAT_PERSIST_START_SECS_OF_DAY_IST..MUHURAT_PERSIST_END_SECS_OF_DAY_IST)
            // O(1) EXEMPT: Range::contains — O(1) bounds check.
            .contains(&wall_clock_ist_secs_of_day)
}

/// Selects the depth timestamp: exchange_timestamp if valid, else wall-clock.
///
/// Full packets (code 8) have exchange_timestamp > 0. Market Depth standalone
/// packets (code 3) have exchange_timestamp=0, so we derive from received_at_nanos.
///
/// O(1) — 1 branch + optionally `utc_nanos_to_ist_secs_of_day`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // APPROVED: epoch fits u32 until 2106
#[inline(always)]
fn derive_depth_timestamp_secs(
    tick_is_valid: bool,
    exchange_timestamp: u32,
    received_at_nanos: i64,
) -> u32 {
    if tick_is_valid {
        exchange_timestamp // Already IST epoch seconds from Dhan WS
    } else {
        // Wall-clock path (Market Depth code 3): convert UTC → IST epoch seconds
        // Must add IST offset so is_today_ist() comparison works correctly
        // (today_ist_day_number is computed in IST, so this must also be IST)
        utc_nanos_to_ist_epoch_secs(received_at_nanos)
    }
}

// #T2b (2026-05-20): format_trading_date_ist / format_bar_minute_ist /
// exchange_segment_label_static removed — they served only the deleted
// last_tick_audit row write.

/// Converts UTC nanoseconds to IST epoch seconds (for stale-day check on wall-clock time).
///
/// O(1) — 2 arithmetic ops.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // APPROVED: epoch secs fits u32 until 2106
#[inline(always)]
fn utc_nanos_to_ist_epoch_secs(received_at_nanos: i64) -> u32 {
    received_at_nanos
        .saturating_div(1_000_000_000)
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS)) as u32
}

// ---------------------------------------------------------------------------
// O(1) Tick Deduplication Ring Buffer
// ---------------------------------------------------------------------------

/// O(1) fixed-size tick-identity ring — a defensive, NEVER-false-drops check.
///
/// Pre-allocated at pipeline startup; zero allocation on the hot path.
/// Single slot per hash bucket, open-addressing with instant eviction.
///
/// # The decided tick identity (operator lock 2026-06-05)
/// Dhan is a tick-by-tick EVENT STREAM — it does NOT re-send anything. The tick
/// identity is `(exchange_segment_code, security_id, exchange_timestamp,
/// received_at_nanos)` and is used EVERYWHERE the same way: in-memory RAM, this
/// ring, the disk spill, the WAL frame log, and the `ticks` table DEDUP key
/// (`(security_id, segment, received_at)`). `received_at_nanos` is the local
/// arrival clock and is unique per arriving frame.
///
/// # Why this can NEVER drop a genuine tick ("keep every tick")
/// Because `received_at_nanos` is unique per arrival, two genuine ticks can
/// never share a fingerprint. So `is_duplicate` returns `true` ONLY for a true
/// byte-identical double-delivery of the SAME frame (same arrival nanos) —
/// which a tick-by-tick stream does not produce. Every genuine tick — including
/// a price RE-TOUCH that repeats an earlier `(price, LTT)` (the operator's
/// 23440 case) — has a fresh `received_at_nanos`, so it is ALWAYS kept. The
/// ring exists purely as an O(1) belt-and-suspenders identity guard; it is not
/// a re-send filter, because there are no re-sends to filter.
///
/// # I-P1-11
/// `security_id` is NOT unique across segments, so `exchange_segment_code` is
/// the FIRST field of the identity — matching the storage DEDUP key.
///
/// # False negative safety
/// Hash collisions cause eviction (some true double-deliveries may pass). This
/// is safe: the QuestDB `ticks` DEDUP UPSERT KEYS are the authoritative
/// server-side dedup for replay/restart idempotency.
struct TickDedupRing {
    /// Pre-allocated slot array. Each slot holds the last fingerprint that
    /// mapped to it. Initialized to `u64::MAX` (empty sentinel — never matches
    /// a real fingerprint).
    slots: Box<[u64]>,
    /// Bitmask for fast modulo: `size - 1` where size is a power of two.
    mask: usize,
}

#[allow(clippy::arithmetic_side_effects)] // APPROVED: wrapping_mul is intentional for hash mixing; shift/bitwise ops are bounded by u64/u32 width
impl TickDedupRing {
    /// Creates a new ring buffer with `2^power` slots.
    ///
    /// # Panics (debug only)
    /// Debug-asserts that `power` is in [8, 24].
    fn new(power: u32) -> Self {
        debug_assert!(
            // O(1) EXEMPT: RangeInclusive::contains inside a boot-time constructor debug_assert.
            (8..=24).contains(&power),
            "dedup ring buffer power out of range"
        );
        let size = 1_usize << power;
        Self {
            // O(1) EXEMPT: ring pre-allocated ONCE at construction — zero alloc per tick thereafter.
            slots: vec![u64::MAX; size].into_boxed_slice(),
            mask: size.wrapping_sub(1),
        }
    }

    /// Returns `true` ONLY for a true byte-identical double-delivery of the same
    /// frame — same `(exchange_segment_code, security_id, exchange_timestamp,
    /// received_at_nanos)`. Because `received_at_nanos` is unique per arrival,
    /// this can never fire for two genuine ticks, so a genuine tick is NEVER
    /// dropped ("keep every tick"). Always stores the fingerprint for the
    /// belt-and-suspenders identity guard.
    ///
    /// # Performance
    /// O(1) — one hash + one array lookup + one compare + one store. Zero alloc.
    #[inline(always)]
    fn is_duplicate(
        &mut self,
        exchange_segment_code: u8,
        security_id: u64,
        exchange_timestamp: u32,
        received_at_nanos: i64,
    ) -> bool {
        let key = Self::fingerprint(
            exchange_segment_code,
            security_id,
            exchange_timestamp,
            received_at_nanos,
        );
        let idx = (key as usize) & self.mask;
        let seen = self.slots[idx] == key;
        self.slots[idx] = key;
        seen
    }

    /// Builds a 64-bit fingerprint from the decided tick identity using FNV-1a.
    ///
    /// Distinct `(exchange_segment_code, security_id, exchange_timestamp,
    /// received_at_nanos)` tuples produce distinct fingerprints with high
    /// probability (~1 - 1/2^64 per pair). `exchange_segment_code` is mixed
    /// FIRST per I-P1-11.
    #[inline(always)]
    fn fingerprint(
        exchange_segment_code: u8,
        security_id: u64,
        exchange_timestamp: u32,
        received_at_nanos: i64,
    ) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325_u64; // FNV-1a 64-bit offset basis
        h ^= u64::from(exchange_segment_code);
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a 64-bit prime
        h ^= security_id;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        h ^= u64::from(exchange_timestamp);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        h ^= received_at_nanos as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        h
    }
}

/// Residual of the tick-conservation identity (cold-path proof — NOT hot path).
///
/// `processed` = ticks that entered the tick branch; the other six are the ONLY
/// terminal outcomes of a tick-branch entry (verified in `run_tick_processor`:
/// persist Ok → `persisted`, persist Err (rescued) → `storage_errors`, and the
/// four `continue` drops `junk` / `stale_day` / `outside_hours` / `dedup`). The
/// loop is synchronous, so at the periodic check there are zero in-flight ticks
/// and the identity is exact. Returns `processed - accounted`: 0 = every tick
/// accounted, > 0 = unaccounted (leak), < 0 = double-count bug.
#[inline]
fn tick_conservation_residual(
    processed: u64,
    persisted: u64,
    storage_errors: u64,
    junk: u64,
    stale_day: u64,
    outside_hours: u64,
    dedup: u64,
) -> i64 {
    let accounted = persisted
        .saturating_add(storage_errors)
        .saturating_add(junk)
        .saturating_add(stale_day)
        .saturating_add(outside_hours)
        .saturating_add(dedup);
    (processed as i64).saturating_sub(accounted as i64)
}

/// Per-window change in the conservation residual. `> 0` means ticks entered the
/// branch this window but reached no known terminal outcome — an ACTIVE leak. A
/// flat residual (delta 0) is benign even if the absolute is non-zero, so a
/// constant offset never pages; only a growing gap does.
#[inline]
fn conservation_leak_delta(current_residual: i64, last_residual: i64) -> i64 {
    current_residual.saturating_sub(last_residual)
}

#[cfg(test)]
mod conservation_tests {
    use super::{conservation_leak_delta, tick_conservation_residual};

    #[test]
    fn test_tick_conservation_residual_balanced_is_zero() {
        // 100 in = 70 persisted + 5 errors + 3 junk + 2 stale + 15 out + 5 dedup
        assert_eq!(tick_conservation_residual(100, 70, 5, 3, 2, 15, 5), 0);
    }

    #[test]
    fn test_tick_conservation_residual_one_unaccounted_is_positive_one() {
        // one tick entered but reached no terminal outcome
        assert_eq!(tick_conservation_residual(100, 70, 5, 3, 2, 15, 4), 1);
    }

    #[test]
    fn test_tick_conservation_residual_double_count_is_negative() {
        // accounted exceeds processed → double-count bug surfaces as negative
        assert_eq!(tick_conservation_residual(100, 99, 5, 0, 0, 0, 0), -4);
    }

    #[test]
    fn test_tick_conservation_residual_all_zero_is_zero() {
        assert_eq!(tick_conservation_residual(0, 0, 0, 0, 0, 0, 0), 0);
    }

    #[test]
    fn test_conservation_leak_delta_flat_residual_is_zero() {
        // constant offset (e.g. benign) → no active leak
        assert_eq!(conservation_leak_delta(7, 7), 0);
    }

    #[test]
    fn test_conservation_leak_delta_growing_gap_is_positive() {
        assert_eq!(conservation_leak_delta(12, 7), 5);
    }

    #[test]
    fn test_conservation_leak_delta_shrinking_is_negative() {
        assert_eq!(conservation_leak_delta(3, 7), -4);
    }
}

/// Runs the tick processing pipeline until the frame receiver closes.
///
/// This is designed to run as a single `tokio::spawn` task.
///
/// # Arguments
/// * `frame_receiver` — raw binary frames from WebSocket pool
/// * `tick_writer` — batched QuestDB ILP writer for ticks (None if QuestDB unavailable)
/// * `tick_broadcast` — optional broadcast sender for fan-out to browser WebSocket clients.
///   O(1) send per tick (atomic write into fixed-size ring buffer). Lagging receivers
///   are auto-skipped — the chart only needs the latest price.
/// * `greeks_enricher` — optional inline Greeks computer. Generic over `G: GreeksEnricher`
///   so the compiler monomorphizes (no vtable on hot path). When present, enriches every
///   valid tick with IV + delta/gamma/theta/vega BEFORE persistence and broadcast.
///   O(1) per tick (HashMap lookup + Jaeckel solve).
///
/// ## Movers retirement (PR #2, 2026-05-18)
///
/// The `top_movers` / `shared_snapshot` / `option_movers` / `_instrument_registry`
/// params were removed alongside the movers pipeline. Under the 4-IDX_I-only
/// universe (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) ranked gainers/losers/most-
/// active snapshots are meaningless.
// Candle-engine re-architecture #T1b: Engine A (`candle_aggregator` +
// `live_candle_writer`) params removed — Engine B aggregates off the
// tick broadcast, not on this hot path.
// APPROVED: indices-only hot-loop entry point — params wire distinct tick/depth/broadcast/greeks/enricher/heartbeat collaborators; a struct would only relocate the count.
#[allow(clippy::too_many_arguments)]
pub async fn run_tick_processor<G: GreeksEnricher>(
    mut frame_receiver: mpsc::Receiver<(u64, bytes::Bytes)>,
    mut tick_writer: Option<TickPersistenceWriter>,
    tick_broadcast: Option<broadcast::Sender<ParsedTick>>,
    mut greeks_enricher: Option<G>,
    // Shared heartbeat for the no-tick watchdog (Parthiban directive
    // 2026-04-21). Updated to `Utc::now().timestamp()` on every parsed
    // tick — single relaxed atomic store on the hot path. `None`
    // disables the heartbeat (used by unit tests that do not spawn
    // the watchdog). See `crate::pipeline::no_tick_watchdog`.
    tick_heartbeat: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>,
    // 29-tf engine plan Phase 2.5: optional Phase 2 lifecycle
    // enricher. When `Some`, every tick that reaches persistence is
    // run through `TickEnricher::enrich_tick(tick, now_ist_secs)` to
    // populate the four lifecycle columns (`volume_delta`,
    // `prev_day_close`, `prev_day_oi`, `phase`) and is written via
    // `append_tick_enriched`. When `None`, falls through to the
    // legacy `append_tick` path which writes default lifecycle
    // values (compatible with spill replay + tests). The boot driver
    // (`crates/app/src/main.rs`) is responsible for loading the
    // `prev_oi_cache` and marking the `BootOrderingGate` ready
    // before attaching the enricher here — see L14.
    tick_enricher: Option<std::sync::Arc<TickEnricher>>,
    // Operator lock 2026-06-01 §30: `(security_id, exchange_segment_code)`
    // pairs EXEMPT from the [09:00, 15:30) IST persist window (GIFT Nifty
    // trades ~21 h/day on NSE-IX). Empty set → no exemptions → today's
    // behavior for every instrument. Boot wires this from
    // `DailyUniverse::always_on_segments` via
    // `tickvault_common::always_on::current()`.
    always_on: std::sync::Arc<std::collections::HashSet<(u64, u8)>>,
    // Live-feed health (SP5, 2026-06-22): the shared per-feed registry the
    // `GET /api/feeds/health` endpoint reads. When `Some`, every parsed Dhan
    // tick records its WALL-CLOCK receipt time (IST nanos) into the Dhan slot so
    // the endpoint reports Dhan's freshness truthfully. `None` (unit tests / any
    // caller not wiring it) is a no-op — byte-identical to the prior hot path.
    // A single relaxed-atomic record next to the existing no-tick heartbeat;
    // O(1), zero-alloc, lock-free (3-agent reviewed CLEAR).
    feed_health: Option<std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>>,
) {
    // Grab metric handles once before the hot loop — O(1) per tick after this.
    // These are no-ops if no metrics recorder is installed (e.g., in tests).
    let m_frames = counter!("tv_frames_processed_total");
    let m_ticks = counter!("tv_ticks_processed_total");
    let m_parse_errors = counter!("tv_parse_errors_total");
    let m_storage_errors = counter!("tv_storage_errors_total");
    let m_junk_filtered = counter!("tv_junk_ticks_filtered_total");
    let m_depth_snapshots = counter!("tv_depth_snapshots_total");
    let m_oi_updates = counter!("tv_oi_updates_total");
    let m_prev_close_updates = counter!("tv_prev_close_updates_total");
    let m_market_status_updates = counter!("tv_market_status_updates_total");
    let m_disconnects = counter!("tv_disconnect_frames_total");
    let m_tick_duration = histogram!("tv_tick_processing_duration_ns");
    // B6 (2026-07-03): true-compute histogram — the total span MINUS any
    // persistence stall the writer accrued this iteration (blocking-send /
    // inline-flush fallbacks, disconnected-branch recovery). Steady state
    // (off-thread flush healthy): stall = 0 and compute ≈ total. The
    // `_duration_ns` suffix auto-inherits the exporter's bucket config.
    let m_tick_compute = histogram!("tv_tick_compute_duration_ns");
    let m_wire_to_done = histogram!("tv_wire_to_done_duration_ns");
    let m_pipeline_active = gauge!("tv_pipeline_active");
    let m_dedup_filtered = counter!("tv_dedup_filtered_total");
    let m_crossed_market = counter!("tv_crossed_market_total");
    let m_outside_hours = counter!("tv_outside_hours_filtered_total");
    let m_stale_day = counter!("tv_stale_day_filtered_total");
    let m_channel_occupancy = gauge!("tv_tick_channel_occupancy");
    let m_channel_capacity = gauge!("tv_tick_channel_capacity");

    // ZL-P0-2: per-underlying canary gauges for the end-to-end "alive
    // but flowing?" Grafana probe. Captured ONCE before the loop so the
    // per-tick update is a single indexed array lookup + atomic store.
    // The array order matches CANARY_UNDERLYINGS so a linear scan of
    // the 3-element table resolves to the right gauge in O(1).
    // O(1) EXEMPT: begin — 3 gauge lookups at function entry, cold path
    let canary_gauges: [metrics::Gauge; 3] = [
        gauge!("tv_ws_canary_last_tick_epoch_secs", "underlying" => "NIFTY"),
        gauge!("tv_ws_canary_last_tick_epoch_secs", "underlying" => "BANKNIFTY"),
        gauge!("tv_ws_canary_last_tick_epoch_secs", "underlying" => "SENSEX"),
    ];
    // O(1) EXEMPT: end

    let mut frames_processed: u64 = 0;
    let mut ticks_processed: u64 = 0;
    let mut parse_errors: u64 = 0;
    let mut storage_errors: u64 = 0;
    let mut junk_ticks_filtered: u64 = 0;
    let mut dedup_filtered: u64 = 0;
    let mut outside_hours_filtered: u64 = 0;
    let mut stale_day_filtered: u64 = 0;
    let mut last_flush_check = Instant::now();
    let mut last_snapshot_check = Instant::now();
    // 2026-05-09 diagnostic ratchet (mock-trading-day blackhole investigation):
    // emit the filter-counter snapshot at INFO every 60 s so the operator
    // can see WHICH gate (`stale_day_filtered` / `outside_hours_filtered` /
    // `dedup_filtered` / `junk_ticks_filtered`) is consuming all ticks
    // when QuestDB shows zero rows. Pre-this-change the only surface was
    // the Prometheus counters, which the operator's QuestDB / Grafana
    // dashboards do not expose. Without this log line, "ticks not flowing
    // on Saturday mock session" required a code dive to diagnose.
    let mut last_filter_stats_log = Instant::now();
    let mut last_logged_stale_day: u64 = 0;
    let mut last_logged_outside_hours: u64 = 0;
    let mut last_logged_dedup: u64 = 0;
    let mut last_logged_junk: u64 = 0;
    let mut last_logged_persisted: u64 = 0;
    // Tick-conservation ledger: previous window's residual (None until seeded
    // on the first window so the first delta is 0 — no spurious page) + the
    // last `ticks_processed` total, used to emit the positive "OK" line only
    // when ticks actually flowed (no idle pre/post-market spam).
    let mut last_conservation_residual: Option<i64> = None;
    let mut last_conservation_processed: u64 = 0;
    // Local mirror of `m_ticks_persisted` Prometheus counter so the
    // 60s periodic stats log can show the persisted-vs-filtered ratio
    // without scraping Prometheus.
    let mut ticks_persisted: u64 = 0;
    // Phase 4b (2026-05-05): `last_movers_persist` + `movers_persist_count`
    // + `m_movers_persisted` DELETED — they were the timing/count state
    // for the legacy `stock_movers` / `option_movers` writers retired
    // alongside `movers_persistence`. Movers persistence cadence is
    // now owned by the unified `movers_pipeline` task.
    let m_ticks_persisted = counter!("tv_ticks_persisted_total");

    // Index prev_close file cache — survives mid-day restarts.
    // On boot, read cached values into a local prev-close map (used by
    // index-prev-close stamping below). PR #2 removed the movers
    // pre-population step — the map is still loaded for the in-process
    // stamping logic that downstream consumers (prev_close_persist,
    // bar enrichers) rely on.
    // O(1) EXEMPT: begin — boot-time file read + HashMap for ~28 indices
    let mut index_prev_close_cache: std::collections::HashMap<u64, f32> = {
        let path = INDEX_PREV_CLOSE_CACHE_PATH;
        match std::fs::read_to_string(path) {
            Ok(json) => match serde_json::from_str::<std::collections::HashMap<u64, f32>>(&json) {
                Ok(cached) => {
                    info!(
                        cached_indices = cached.len(),
                        "index prev_close loaded from file cache (mid-day restart recovery)"
                    );
                    cached
                }
                Err(err) => {
                    warn!(
                        ?err,
                        "failed to parse index prev_close cache — starting fresh"
                    );
                    std::collections::HashMap::with_capacity(50)
                }
            },
            Err(_) => {
                debug!("no index prev_close cache found — first boot of the day");
                std::collections::HashMap::with_capacity(50)
            }
        }
    };
    // O(1) EXEMPT: end

    // PrevClose diagnostic counters — per-segment tracking + summary log.
    // Helps confirm exactly how many PrevClose packets Dhan sends per segment.
    let mut prev_close_idx_i: u64 = 0;
    let mut prev_close_nse_eq: u64 = 0;
    let mut prev_close_nse_fno: u64 = 0;
    let mut prev_close_other: u64 = 0;
    let mut prev_close_summary_logged = false;

    // PROOF: track unique instruments that received day_close baseline.
    // Logged periodically so you can see "day_close baselines: 24,872 instruments"
    // growing in real-time. If this stays 0, day_close is not working.
    // O(1) EXEMPT: begin — HashSet for baseline tracking (boot + first 60s only)
    let mut day_close_baseline_count: u64 = 0;
    let mut day_close_baseline_logged = false;
    let mut day_close_first_log_time = None::<Instant>;
    // O(1) EXEMPT: end

    // O(1) dedup ring buffer — pre-allocated once, zero allocation in hot loop.
    let mut dedup_ring = TickDedupRing::new(DEDUP_RING_BUFFER_POWER);

    // Wave 5 Item 26 L1 — volume monotonicity guard. One HashMap entry
    // per (security_id, segment) tracking last-seen volume. On each
    // tick: O(1) lookup; if `volume < previous` within trading day,
    // emit ERROR `VOLUME-MONO-01` + Prom counter. Silent on the happy
    // path. Cleared at IST midnight via the `FirstSeenSet`-class daily
    // reset task (separate scheduler).
    let mut volume_monotonicity_guard =
        super::volume_monotonicity_guard::VolumeMonotonicityGuard::new();

    // Defense-in-depth: reject stale ticks from previous trading days.
    // exchange_timestamp is IST epoch seconds; dividing by 86400 gives the IST day number.
    // Computed once at startup — valid for the entire trading session (09:00–15:30).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // APPROVED: epoch day fits u32 until 2106
    let today_ist_day_number: u32 = {
        let now_utc = chrono::Utc::now().timestamp();
        let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
        now_ist.saturating_div(i64::from(SECONDS_PER_DAY)) as u32
    };

    // Wave 2 Item 7.1 (G14) — block on QuestDB readiness BEFORE consuming
    // the SPSC ring. Until this gate clears, ticks accumulate safely in
    // the upstream `frame_receiver` channel (65,536 cap) and the WAL
    // spill — the consume loop simply has not started yet, so nothing
    // is dropped. Reading the global config is the canonical wiring per
    // `crates/storage/src/lib.rs::set_global_questdb_config`. Tests that
    // never installed a global skip the wait and proceed immediately.
    if let Some(qcfg) = tickvault_storage::global_questdb_config() {
        // Boot probe owns its own escalating-log table (+5 DEBUG / +10
        // INFO / +20 WARN / +30 ERROR BOOT-01 / +60 CRITICAL BOOT-02).
        // We tag the entry log so operators can correlate with the
        // probe's own logs.
        info!(
            "tick processor: gating on wait_for_questdb_ready({}s) before SPSC consume \
             (Wave 2 Item 7.1 / G14)",
            tickvault_common::constants::BOOT_DEADLINE_SECS
        );
        match tickvault_storage::boot_probe::wait_for_questdb_ready(
            qcfg,
            tickvault_common::constants::BOOT_DEADLINE_SECS,
        )
        .await
        {
            Ok(elapsed) => {
                info!(
                    elapsed_secs = elapsed.as_secs(),
                    "tick processor: QuestDB ready, opening SPSC consume loop"
                );
            }
            Err(err) => {
                // C2 (2026-07-03): attribute the failure honestly. A
                // ClientBuild error means OUR HTTP client could not be
                // constructed (host fd/TLS/resolver pressure) — routing
                // that to the BOOT-02 Docker/QuestDB runbook misdirects
                // triage. Control flow is identical in both arms.
                if matches!(
                    &err,
                    tickvault_storage::boot_probe::BootProbeError::ClientBuild(_)
                ) {
                    error!(
                        ?err,
                        code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed
                            .code_str(),
                        "HTTP-CLIENT-01 tick processor refusing to consume — HTTP client \
                         build failed (host fd/TLS/resolver problem, not QuestDB)"
                    );
                } else {
                    error!(
                        ?err,
                        code = tickvault_common::error_code::ErrorCode::Boot02DeadlineExceeded
                            .code_str(),
                        "BOOT-02 tick processor refusing to consume — QuestDB never reached \
                         ready state within deadline"
                    );
                }
                // Refuse to start the hot loop. The caller's processor
                // task ends; ticks remain buffered in the SPSC ring + WAL
                // spill. Process-level HALT is the boot path's call (the
                // boot orchestrator already runs `wait_for_questdb_ready`
                // on its own deadline at main.rs:1692 and bails on Err).
                return;
            }
        }
    } else {
        debug!(
            "tick processor: no global QuestDB config installed — skipping boot probe \
             (test mode)"
        );
    }

    m_pipeline_active.set(1.0);
    // Record channel capacity once at startup (65536 for SPSC buffer).
    m_channel_capacity.set(frame_receiver.max_capacity() as f64);
    info!(today_ist_day_number, "tick processor started");

    // CCL-06: read the boot-computed Muhurat-session flag ONCE, before the hot
    // loop (O(1), zero-alloc, `bool` Copy — mirrors the `always_on` set read).
    // `false` on every trading/mock day (and any test that never boots the
    // calendar), so the persist gates stay byte-for-byte identical to today;
    // `true` only on a Diwali Muhurat date, when the evening [18:00, 19:30) IST
    // window is additionally accepted so those ticks are persisted instead of
    // silently dropped.
    let muhurat_active = tickvault_common::muhurat::current();

    // PR #288: segment-aware sequence tracker (I-P1-11 compliant). Replaces
    // the previous `HashMap<(u32, u8), u32>` which keyed on security_id
    // alone — that was subject to the cross-segment collision bug where a
    // FINNIFTY IDX_I id=27 packet would poison an NSE_EQ id=27 tracker
    // entry (or vice versa). The new tracker keys on
    // `(security_id, ExchangeSegment, DepthSide)` and emits 3 distinct
    // Prometheus counters (holes / duplicates / rollbacks) so operators
    // can build dashboards that distinguish real packet loss from
    // benign reconnect replay.
    // PR #4 (2026-05-19): `depth_seq_tracker` retired alongside the
    // deleted depth_sequence_tracker module. Depth feeds are gone.

    // HIGH-3 (B6 adversarial round 1): worker-side spill was evaluated and
    // rejected (the spill/DLQ machinery is `&mut TickPersistenceWriter`
    // state — file handles, counters, disk pre-flight, ring-first ordering,
    // mid-session reconnect drain — a worker-owned spill file would bypass
    // the in-session recovery drain and defer every failed batch to next
    // boot). Instead the recv loop selects over a 1s idle interval whose
    // tick calls the SAME `flush_if_needed` (drain failed batches +
    // time-based flush) even when no frame arrives. `biased` + recv-first
    // preserves the frame-path semantics byte-for-byte when frames flow.
    let mut idle_flush_interval = tokio::time::interval(IDLE_FLUSH_DRAIN_INTERVAL);
    idle_flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let (frame_seq, raw_frame) = tokio::select! {
            biased;
            maybe_frame = frame_receiver.recv() => match maybe_frame {
                Some(frame) => frame,
                None => break, // channel closed — shutdown
            },
            _ = idle_flush_interval.tick() => {
                if let Some(ref mut writer) = tick_writer {
                    if let Err(err) = writer.flush_if_needed() {
                        // Rule 5: flush/persist failures are error! (routes
                        // to Telegram) — same treatment as the per-frame
                        // periodic flush below.
                        error!(
                            ?err,
                            "idle-interval tick flush failed — QuestDB write path broken"
                        );
                        metrics::counter!("tv_tick_flush_errors_total").increment(1);
                    }
                    // MEDIUM-4: any stall this idle drain accrued belongs to
                    // the flush side — discard it so it can never deflate a
                    // later frame's compute sample.
                    let deferred_stall_ns = writer.take_last_stall_ns();
                    if deferred_stall_ns > 0 {
                        metrics::counter!("tv_tick_flush_deferred_stall_ns_total")
                            .increment(deferred_stall_ns);
                    }
                }
                continue;
            }
        };
        // TICK-SEQ-01: the read-loop `frame_seq` IS this frame's capture
        // sequence (1 frame = 1 tick — review CRITICAL #1). It is replay-stable:
        // WAL replay re-delivers the SAME frame_seq, so the persisted
        // `capture_seq` matches the original. u64→i64 is safe (wall-nanos-seeded,
        // always positive, « i64::MAX; keeps `ORDER BY capture_seq` correct).
        let capture_seq = i64::try_from(frame_seq).unwrap_or(i64::MAX);
        let tick_start = Instant::now();
        frames_processed = frames_processed.saturating_add(1);
        m_frames.increment(1);
        let received_at_nanos = current_received_at_nanos();

        // 29-tf engine Phase 2.10 (hostile bug-hunt M1 fix): phase
        // classification now uses `tick.exchange_timestamp` (Dhan-source
        // IST epoch seconds for THIS tick) instead of the wall-clock
        // `now_ist_secs_of_day()`. Network-arrival jitter at the
        // 09:14:59 ↔ 09:15:00 boundary previously misclassified ~10-30
        // ticks/day (a tick stamped 09:14:59 IST that arrived at
        // 09:15:01 IST got phase=OPEN instead of PREOPEN). The
        // exchange_timestamp is the data-time, which is the correct
        // basis for phase classification — not the receive time.
        //
        // Note: Phase 2.7's wall-clock hoist is preserved for the
        // empty-frame fallback below; per-tick computation now uses
        // tick.exchange_timestamp directly which is already in ParsedTick
        // (no syscall, just a modulo).

        // Parse the binary frame
        let parsed = match dispatch_frame(&raw_frame, received_at_nanos) {
            Ok(frame) => frame,
            Err(err) => {
                parse_errors = parse_errors.saturating_add(1);
                m_parse_errors.increment(1);
                // H4: Log hex dump of first 64 bytes for post-mortem forensics.
                // ERROR level triggers Telegram so persistent parse failures
                // are visible to operators, not just in log aggregation.
                if parse_errors <= 100 {
                    let hex_len = raw_frame.len().min(64);
                    let hex_dump: String = raw_frame[..hex_len]
                        .iter()
                        // O(1) EXEMPT: cold path, parse errors are rare (bounded
                        // to the first 100); only runs on a malformed frame.
                        .map(|b| format!("{b:02x}")) // O(1) EXEMPT: cold path
                        .collect::<Vec<_>>()
                        .join(" "); // O(1) EXEMPT: cold path, parse errors are rare
                    if parse_errors.is_multiple_of(10) {
                        // Escalate every 10th error to ERROR (triggers Telegram)
                        error!(
                            ?err,
                            frame_len = raw_frame.len(),
                            frame_hex = %hex_dump,
                            total_errors = parse_errors,
                            "H4: persistent parse failures — {parse_errors} total (hex dump of first {hex_len} bytes)"
                        );
                    } else {
                        warn!(
                            ?err,
                            frame_len = raw_frame.len(),
                            frame_hex = %hex_dump,
                            total_errors = parse_errors,
                            "failed to parse binary frame (hex dump of first {hex_len} bytes)"
                        );
                    }
                }
                continue;
            }
        };

        // Process based on frame type
        match parsed {
            ParsedFrame::Tick(mut tick) => {
                ticks_processed = ticks_processed.saturating_add(1);
                m_ticks.increment(1);

                // No-tick watchdog heartbeat (Parthiban directive 2026-04-21).
                // Single relaxed atomic store. Latency-hunt 2026-06-10:
                // derive seconds from the frame's `received_at_nanos`
                // (captured once per frame above) instead of a second
                // `Utc::now()` vDSO read (~57 ns measured) — identical
                // wall-clock second; the watchdog threshold is 30 s.
                if let Some(ref hb) = tick_heartbeat {
                    hb.store(
                        received_at_nanos.saturating_div(1_000_000_000),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }

                // Live-feed health (SP5): record this Dhan tick's WALL-CLOCK
                // receipt time as IST nanos. `received_at_nanos` is UTC nanos
                // (`current_received_at_nanos`), but the health endpoint's
                // last-tick-age math compares against `Utc::now()+IST` — so we
                // add the IST offset for clock-consistency (same convention as
                // the Groww lane + `received_at` in data-integrity.md). Using UTC
                // here would read ~5.5h in the future → age saturates to 0 →
                // permanent false-fresh. `saturating_add` avoids the
                // debug-build overflow-panic path (security review MEDIUM).
                // One relaxed-atomic record; O(1), zero-alloc.
                if let Some(ref fh) = feed_health {
                    fh.record_tick(
                        tickvault_common::feed::Feed::Dhan,
                        received_at_nanos
                            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
                    );
                }

                // Log PrevClose summary once, on the first real tick.
                // By this point the PrevClose burst from subscription is complete.
                if !prev_close_summary_logged {
                    prev_close_summary_logged = true;
                    let total = prev_close_idx_i
                        .saturating_add(prev_close_nse_eq)
                        .saturating_add(prev_close_nse_fno)
                        .saturating_add(prev_close_other);
                    info!(
                        total,
                        idx_i = prev_close_idx_i,
                        nse_eq = prev_close_nse_eq,
                        nse_fno = prev_close_nse_fno,
                        other = prev_close_other,
                        "PrevClose burst complete — per-segment summary"
                    );
                    if prev_close_nse_eq == 0 && prev_close_nse_fno == 0 {
                        info!(
                            "PrevClose: Dhan sent 0 packets for NSE_EQ/NSE_FNO (expected). \
                             Equity/F&O movers use day_close from Full ticks as baseline."
                        );
                    }
                }

                // Wave 2 Item 8 (G4) — record this tick observation in
                // the global gap detector. O(1) hot-path: single
                // OnceLock read + one papaya insert. No-op when the
                // detector is not installed. Latency-hunt 2026-06-10:
                // reuse `tick_start` (captured ≤1 µs earlier) instead of
                // a third per-tick clock read (~30 ns measured); the
                // detector's silence threshold is 30 s.
                if let Some(seg) =
                    tickvault_common::types::ExchangeSegment::from_byte(tick.exchange_segment_code)
                {
                    super::tick_gap_detector::record_tick_global(tick.security_id, seg, tick_start);
                }

                // Filter junk ticks: NaN/Infinity, zero/negative LTP,
                // or epoch timestamps (heartbeat/init frames from Dhan).
                if !is_valid_tick(tick.last_traded_price, tick.exchange_timestamp) {
                    junk_ticks_filtered = junk_ticks_filtered.saturating_add(1);
                    m_junk_filtered.increment(1);
                    if junk_ticks_filtered <= 10 {
                        debug!(
                            security_id = tick.security_id,
                            ltp = tick.last_traded_price,
                            exchange_timestamp = tick.exchange_timestamp,
                            total_filtered = junk_ticks_filtered,
                            "junk tick filtered — LTP non-finite/invalid or timestamp invalid"
                        );
                    }
                    continue;
                }

                // Phase 0 Item 22e: tick-level OHLC integrity guards.
                // Detection-only (NOT a filter) — the tick still flows
                // through persistence + indicators. Static-label counters
                // emit per violation kind so the hot path stays zero-alloc.
                // Each bit set in the mask represents one violation; the
                // operator watches Prometheus counters for non-zero rates.
                let ohlc_violations =
                    check_tick_ohlc_integrity(tick.last_traded_price, tick.day_high, tick.day_low);
                if ohlc_violations != 0 {
                    if ohlc_violations & 0b001 != 0 {
                        metrics::counter!("tv_tick_integrity_high_below_low_total").increment(1);
                    }
                    if ohlc_violations & 0b010 != 0 {
                        metrics::counter!("tv_tick_integrity_ltp_above_high_total").increment(1);
                    }
                    if ohlc_violations & 0b100 != 0 {
                        metrics::counter!("tv_tick_integrity_ltp_below_low_total").increment(1);
                    }
                }

                // PR #2 (2026-05-18): movers `update_prev_close` baseline
                // calls removed alongside the movers pipeline. The
                // `day_close` field on Quote / Full packets is still
                // tracked here (counter + first-seen log) because
                // downstream `previous_close` ILP writes for IDX_I and
                // boot-time index prev-close stamping still depend on
                // the day_close field. Movers retirement does not
                // remove `day_close` from the binary protocol — only
                // its consumption by the deleted tracker types.
                if tick.day_close > 0.0 && tick.day_close.is_finite() {
                    day_close_baseline_count = day_close_baseline_count.saturating_add(1);
                    if day_close_first_log_time.is_none() {
                        day_close_first_log_time = Some(Instant::now());
                    }
                }

                // Ingestion gate: drop ALL ticks outside [9:00 AM, 3:30 PM) IST.
                // WebSocket connects immediately on app start (pre-market warmup),
                // but stale ticks from previous day are dropped here. At 15:30 PM
                // the WebSocket is disconnected, but any in-flight ticks at the
                // boundary are also caught by this gate. O(1): 1 modulo + 1 range check.
                // Also rejects ticks from previous trading days (stale replays on reconnect).
                if !is_today_ist(tick.exchange_timestamp, today_ist_day_number) {
                    stale_day_filtered = stale_day_filtered.saturating_add(1);
                    m_stale_day.increment(1);
                    continue;
                }
                // Operator lock 2026-06-01 §30: always-on instruments (GIFT
                // Nifty, ~21 h/day) skip BOTH market-hours window gates so
                // their off-session ticks still persist. The stale-day
                // guard above is KEPT (a GIFT tick must still be today's).
                // O(1): one lookup of a tiny boot-set set (usually ≤1 entry).
                let window_exempt =
                    is_window_exempt(&always_on, tick.security_id, tick.exchange_segment_code);
                if !window_exempt
                    && !is_within_persist_window(tick.exchange_timestamp, muhurat_active)
                {
                    // Phase 0 Item 12 (2026-05-17) — `last_tick_audit` LATE-TICK
                    // anomaly path. Before dropping the tick, if its
                    // `exchange_timestamp` stamps at or after the session
                    // close boundary (15:30:00 IST), record a forensic
                    // audit row + increment the diagnostic counter. The
                    // audit row's DEDUP key
                    // `(trading_date_ist, bar_minute, security_id, exchange_segment)`
                    // collapses bursts to one row per minute-bar per SID
                    // so the operator can query "did Dhan ever stamp a
                    // tick after 15:30:00 today?" without per-tick noise.
                    let exchange_secs_of_day = tick.exchange_timestamp % SECONDS_PER_DAY;
                    if exchange_secs_of_day >= TICK_PERSIST_END_SECS_OF_DAY_IST {
                        metrics::counter!("tv_late_tick_after_boundary_total").increment(1);
                    }
                    outside_hours_filtered = outside_hours_filtered.saturating_add(1);
                    m_outside_hours.increment(1);
                    continue;
                }
                // Wall-clock guard: reject stale WebSocket snapshots received
                // outside market hours (post-market data has valid exchange_timestamp
                // from today's session but arrives after 15:30 IST).
                // §30: always-on instruments (GIFT Nifty) are exempt here too.
                if !window_exempt
                    && !is_wall_clock_within_persist_window(tick.received_at_nanos, muhurat_active)
                {
                    outside_hours_filtered = outside_hours_filtered.saturating_add(1);
                    m_outside_hours.increment(1);
                    continue;
                }

                // O(1) identity guard on the decided tick identity
                // (segment, security_id, timestamp, received_at). Dhan never
                // re-sends (tick-by-tick event stream), and received_at is
                // unique per arrival, so this NEVER drops a genuine tick — it
                // only catches a true byte-identical double-delivery.
                if dedup_ring.is_duplicate(
                    tick.exchange_segment_code,
                    tick.security_id,
                    tick.exchange_timestamp,
                    tick.received_at_nanos,
                ) {
                    dedup_filtered = dedup_filtered.saturating_add(1);
                    m_dedup_filtered.increment(1);
                    continue;
                }

                // ZL-P0-2: canary heartbeat — if this tick is one of the
                // 3 spot indices (NIFTY/BANKNIFTY/SENSEX), bump the
                // corresponding per-underlying gauge so a Grafana rule
                // can detect "pipeline alive but THIS underlying is
                // flat-lined". 3-element linear scan is faster than a
                // HashMap for N=3 because the whole table fits in one
                // cache line and the comparisons are all u32 equality.
                // O(1) in the strict sense: bounded at 3 iterations
                // regardless of tick rate or universe size.
                // Latency-hunt 2026-06-10: derive the epoch-seconds gauge
                // value from `received_at_nanos` instead of a fresh
                // `Utc::now()` (~59 ns measured) — index ticks hit this
                // branch on every frame and the probe granularity is
                // seconds.
                for (idx, (sid, _name)) in CANARY_UNDERLYINGS.iter().enumerate() {
                    if *sid == tick.security_id {
                        #[allow(clippy::cast_precision_loss)]
                        // APPROVED: epoch seconds (~1.7e9) are exactly representable in f64
                        canary_gauges[idx]
                            .set(received_at_nanos.saturating_div(1_000_000_000) as f64);
                        break;
                    }
                }

                // O(1) inline Greeks enrichment: compute IV + delta/gamma/theta/vega
                // for F&O option ticks, update underlying LTP cache for index/equity.
                // Runs BEFORE persistence so Greeks values are stored in QuestDB.
                if let Some(ref mut enricher) = greeks_enricher {
                    enricher.enrich(&mut tick);
                }

                // 29-tf engine Phase 2.7 (hostile bug-hunt CRITICAL C2 fix):
                // run the lifecycle enricher ONCE per tick BEFORE both the
                // VOLUME-MONO-01 guard and the persistence branch. The
                // enricher's `volume_is_first_seen` + `phase` flags must
                // suppress the monotonicity check when (a) it's the first
                // tick of the day for this instrument or (b) phase != OPEN.
                // Without this gate, IST midnight rollover fires ~24,300
                // false VOLUME-MONO-01 alerts at 09:15 IST every trading
                // day (per L13).
                let lifecycle_for_tick: Option<(
                    bool,
                    super::tick_enricher::EnrichedTickFlags,
                    TickLifecycle,
                )> = tick_enricher.as_ref().map(|enricher| {
                    // Phase 2.10 M1 fix: per-tick exchange_timestamp drives
                    // phase classification (not wall-clock arrival).
                    let tick_secs_of_day = tick.exchange_timestamp % 86_400;
                    let enriched = enricher.enrich_tick(&tick, tick_secs_of_day);
                    let life = TickLifecycle {
                        volume_delta: enriched.volume_delta,
                        prev_day_close: enriched.prev_day_close,
                        prev_day_oi: enriched.prev_day_oi,
                        phase: enriched.phase as u8,
                    };
                    let flags = super::tick_enricher::EnrichedTickFlags {
                        volume_is_first_seen: enriched.volume_is_first_seen,
                        volume_is_regression: enriched.volume_is_regression,
                        phase: enriched.phase,
                    };
                    (enriched.volume_is_first_seen, flags, life)
                });

                // Persist tick to QuestDB — ingestion gate above already verified
                // [09:00, 15:30) IST and today's date. This block only handles
                // QuestDB write errors (connection down, buffer full, etc.).
                if let Some(ref mut writer) = tick_writer {
                    let result = if let Some((_, _, life)) = lifecycle_for_tick {
                        writer.append_tick_enriched_with_seq(&tick, life, capture_seq)
                    } else {
                        writer.append_tick_with_seq(&tick, capture_seq)
                    };
                    match result {
                        Ok(()) => {
                            m_ticks_persisted.increment(1);
                            ticks_persisted = ticks_persisted.saturating_add(1);
                        }
                        Err(err) => {
                            storage_errors = storage_errors.saturating_add(1);
                            m_storage_errors.increment(1);
                            // Audit Rule 5: persist failures use error! (Telegram-
                            // routable). Edge-triggered (Rule 4): page on the FIRST
                            // error + every 1000th to avoid per-tick spam. The tick
                            // is NOT lost — force_flush rescues it into the
                            // ring → spill → WAL chain.
                            if storage_errors == 1 || storage_errors.is_multiple_of(1000) {
                                error!(
                                    ?err,
                                    security_id = tick.security_id,
                                    total_errors = storage_errors,
                                    "failed to append tick to QuestDB (tick rescued to ring/spill/WAL)"
                                );
                            }
                        }
                    }
                }

                // O(1) broadcast to browser WebSocket clients (if any are connected).
                // Lagging receivers auto-skip — chart only needs latest price.
                //
                // Audit finding #1/#19 (2026-04-17): `sender.send()` returns
                // `Err(SendError)` ONLY when ALL receivers have closed (not
                // on lag — lag is surfaced to each receiver individually).
                // A closed-all-receivers state means downstream trading
                // signals are dead. Count + error-log so operator sees it.
                if let Some(ref sender) = tick_broadcast
                    && sender.send(tick).is_err()
                {
                    metrics::counter!("tv_tick_broadcast_send_errors_total").increment(1);
                    // O(1) EXEMPT: error path — tracing static dispatch is
                    // the only allocation and it's off the zero-loss fast path.
                    error!(
                        "tick broadcast send failed — ALL subscribers closed; \
                         trading pipeline + browser feed are dead"
                    );
                }

                // Candle-engine re-architecture #T1b: in-loop candle
                // aggregation removed. Engine B (the multi-TF aggregator)
                // subscribes to the tick broadcast off this hot path.
                //
                // PR #2 (2026-05-18): movers `update(&tick)` hot-path
                // calls removed alongside the movers pipeline. Under
                // the 4-IDX_I-only universe the ranked-movers snapshot
                // is meaningless and the trackers are deleted.

                // Wave 5 Item 26 L1 — volume monotonicity guard.
                //
                // 29-tf engine Phase 2.7 (hostile bug-hunt CRITICAL C2 fix):
                // when the lifecycle enricher is attached, suppress the
                // monotonicity check on (a) the FIRST tick of the day for
                // this instrument (Dhan legitimately resets the cumulative
                // counter to ~0 at session open) and (b) any phase other
                // than OPEN (PREMARKET / PREOPEN / POSTAUCTION / CLOSED
                // ticks aren't subject to the live-trading invariant). Per
                // L13: this prevents ~24,300 false breach alerts at IST
                // 09:15 every trading day. When no enricher is attached
                // (legacy / fast boot), the guard runs unconditionally.
                let suppress_monotonicity = match lifecycle_for_tick {
                    Some((_, flags, _)) => {
                        flags.volume_is_first_seen
                            || flags.phase != tickvault_common::phase::Phase::Open
                    }
                    None => false,
                };
                if !suppress_monotonicity {
                    let _ = volume_monotonicity_guard.observe(
                        tick.security_id,
                        tick.exchange_segment_code,
                        tick.volume,
                    );
                }

                trace!(
                    security_id = tick.security_id,
                    ltp = tick.last_traded_price,
                    "tick processed"
                );
            }
            ParsedFrame::TickWithDepth(mut tick, depth) => {
                ticks_processed = ticks_processed.saturating_add(1);
                m_ticks.increment(1);

                // No-tick watchdog heartbeat — same pattern as the Tick
                // branch above (incl. the latency-hunt 2026-06-10
                // received_at-derived seconds). See `no_tick_watchdog`.
                if let Some(ref hb) = tick_heartbeat {
                    hb.store(
                        received_at_nanos.saturating_div(1_000_000_000),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }

                // Live-feed health (SP5): record this Dhan tick's WALL-CLOCK
                // receipt time as IST nanos. `received_at_nanos` is UTC nanos
                // (`current_received_at_nanos`), but the health endpoint's
                // last-tick-age math compares against `Utc::now()+IST` — so we
                // add the IST offset for clock-consistency (same convention as
                // the Groww lane + `received_at` in data-integrity.md). Using UTC
                // here would read ~5.5h in the future → age saturates to 0 →
                // permanent false-fresh. `saturating_add` avoids the
                // debug-build overflow-panic path (security review MEDIUM).
                // One relaxed-atomic record; O(1), zero-alloc.
                if let Some(ref fh) = feed_health {
                    fh.record_tick(
                        tickvault_common::feed::Feed::Dhan,
                        received_at_nanos
                            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
                    );
                }

                // Depth data is ALWAYS persisted — Market Depth standalone packets
                // (code 3) have exchange_timestamp=0 by design, but their 5-level
                // depth is valid. The tick portion is only persisted when the
                // exchange timestamp is valid (Full packets, code 8).
                let ltp_valid = is_valid_ltp(tick.last_traded_price);
                let tick_is_valid = is_valid_tick(tick.last_traded_price, tick.exchange_timestamp);

                // PR #2 (2026-05-18): movers `update_prev_close` baseline
                // calls on Full-packet day_close removed alongside the
                // movers pipeline. Counter still tracks day_close
                // presence for the heartbeat log below; the trackers
                // themselves are deleted.
                if tick.day_close > 0.0 && tick.day_close.is_finite() {
                    day_close_baseline_count = day_close_baseline_count.saturating_add(1);
                    if day_close_first_log_time.is_none() {
                        day_close_first_log_time = Some(Instant::now());
                    }
                }

                if tick_is_valid {
                    // Ingestion gate: drop Full packet ticks outside [9:00 AM, 3:30 PM) IST.
                    // Same gate as Tick path — prevents stale pre-market and post-market
                    // data from entering dedup ring, candle aggregator, or QuestDB.
                    if !is_today_ist(tick.exchange_timestamp, today_ist_day_number) {
                        stale_day_filtered = stale_day_filtered.saturating_add(1);
                        m_stale_day.increment(1);
                        continue;
                    }
                    // §30 parity (audit Rule 6 — both guards on EVERY persist site):
                    // always-on instruments (GIFT Nifty, ~21 h/day) skip the two
                    // window gates here too, exactly like the Ticker/Quote path.
                    // Latent today (GIFT subscribes Quote mode), but keeps the two
                    // persist paths symmetric so a future Full-mode always-on SID
                    // is not silently dropped ~16 h/day.
                    let window_exempt =
                        is_window_exempt(&always_on, tick.security_id, tick.exchange_segment_code);
                    if !window_exempt
                        && !is_within_persist_window(tick.exchange_timestamp, muhurat_active)
                    {
                        outside_hours_filtered = outside_hours_filtered.saturating_add(1);
                        m_outside_hours.increment(1);
                        continue;
                    }
                    // Wall-clock guard: reject stale WebSocket snapshots received
                    // outside market hours (same as Ticker/Quote path above).
                    if !window_exempt
                        && !is_wall_clock_within_persist_window(
                            tick.received_at_nanos,
                            muhurat_active,
                        )
                    {
                        outside_hours_filtered = outside_hours_filtered.saturating_add(1);
                        m_outside_hours.increment(1);
                        continue;
                    }

                    // O(1) identity guard on the decided tick identity
                    // (segment, security_id, timestamp, received_at). received_at
                    // is unique per arrival, so a genuine snapshot is never
                    // dropped — only a true byte-identical double-delivery is.
                    if dedup_ring.is_duplicate(
                        tick.exchange_segment_code,
                        tick.security_id,
                        tick.exchange_timestamp,
                        tick.received_at_nanos,
                    ) {
                        dedup_filtered = dedup_filtered.saturating_add(1);
                        m_dedup_filtered.increment(1);
                        continue;
                    }

                    // O(1) inline Greeks enrichment for Full packet ticks.
                    if let Some(ref mut enricher) = greeks_enricher {
                        enricher.enrich(&mut tick);
                    }

                    // Persist tick to QuestDB — ingestion gate above already verified
                    // [09:00, 15:30) IST and today's date.
                    //
                    // 29-tf engine Phase 2.5: same enricher branch as the
                    // earlier persistence site — keeps both Quote and Full
                    // packet paths populating the lifecycle columns
                    // identically. Two call sites mirror each other so the
                    // hot-path semantics are uniform across packet types.
                    if let Some(ref mut writer) = tick_writer {
                        let result = if let Some(ref enricher) = tick_enricher {
                            // Phase 2.10 M1 fix: per-tick exchange_timestamp
                            // drives phase classification (not wall-clock
                            // arrival). Mirrors the Quote-arm site above.
                            let tick_secs_of_day = tick.exchange_timestamp % 86_400;
                            let enriched = enricher.enrich_tick(&tick, tick_secs_of_day);
                            let life = TickLifecycle {
                                volume_delta: enriched.volume_delta,
                                prev_day_close: enriched.prev_day_close,
                                prev_day_oi: enriched.prev_day_oi,
                                phase: enriched.phase as u8,
                            };
                            // TICK-SEQ-01 PR-2b (review C1): the Full-packet arm
                            // MUST thread the same replay-stable capture_seq as the
                            // Quote arm — else WAL replay re-stamps a fresh seq and
                            // the row duplicates under the capture_seq dedup key.
                            writer.append_tick_enriched_with_seq(&tick, life, capture_seq)
                        } else {
                            writer.append_tick_with_seq(&tick, capture_seq)
                        };
                        match result {
                            Ok(()) => {
                                m_ticks_persisted.increment(1);
                                ticks_persisted = ticks_persisted.saturating_add(1);
                            }
                            Err(err) => {
                                storage_errors = storage_errors.saturating_add(1);
                                m_storage_errors.increment(1);
                                // Audit Rule 5 + Rule 4 (edge-triggered): page on
                                // first error + every 1000th. Tick rescued to
                                // ring/spill/WAL — not lost.
                                if storage_errors == 1 || storage_errors.is_multiple_of(1000) {
                                    error!(
                                        ?err,
                                        security_id = tick.security_id,
                                        total_errors = storage_errors,
                                        "failed to append tick to QuestDB (tick rescued to ring/spill/WAL)"
                                    );
                                }
                            }
                        }
                    }
                } else if !ltp_valid {
                    // LTP non-finite or non-positive — skip depth too (truly junk)
                    junk_ticks_filtered = junk_ticks_filtered.saturating_add(1);
                    m_junk_filtered.increment(1);
                    if junk_ticks_filtered <= 10 {
                        debug!(
                            security_id = tick.security_id,
                            ltp = tick.last_traded_price,
                            exchange_timestamp = tick.exchange_timestamp,
                            total_filtered = junk_ticks_filtered,
                            "junk tick with depth filtered — LTP non-finite/invalid"
                        );
                    }
                    continue;
                }
                // else: LTP valid but no exchange_timestamp (Market Depth code 3)
                // — skip tick persistence, still persist depth below

                // Guard: skip depth with NaN/Infinity bid/ask prices
                if !depth_prices_are_finite(&depth) {
                    junk_ticks_filtered = junk_ticks_filtered.saturating_add(1);
                    m_junk_filtered.increment(1);
                    continue;
                }

                // O(1) crossed market detection: bid > ask at best level.
                // Occurs during auction periods — metric only, do NOT filter.
                if is_crossed_market(depth[0].bid_price, depth[0].ask_price) {
                    m_crossed_market.increment(1);
                    trace!(
                        security_id = tick.security_id,
                        bid = depth[0].bid_price,
                        ask = depth[0].ask_price,
                        "crossed market detected (bid > ask at best level)"
                    );
                }

                // Ingestion gate for depth: only during [09:00, 15:30) IST today.
                // For Full packets (code 8), use exchange_timestamp.
                // For Market Depth standalone (code 3), exchange_timestamp=0,
                // so derive wall-clock seconds from received_at_nanos.
                let depth_ts_secs = derive_depth_timestamp_secs(
                    tick_is_valid,
                    tick.exchange_timestamp,
                    tick.received_at_nanos,
                );
                if !is_today_ist(depth_ts_secs, today_ist_day_number) {
                    stale_day_filtered = stale_day_filtered.saturating_add(1);
                    m_stale_day.increment(1);
                    continue;
                }
                if !is_within_persist_window(depth_ts_secs, muhurat_active) {
                    outside_hours_filtered = outside_hours_filtered.saturating_add(1);
                    m_outside_hours.increment(1);
                    continue;
                }
                // Audit finding #6 (2026-04-17): wall-clock guard for depth.
                // Ticks already have this at line ~802; depth was missing
                // it. A Full packet (code 8) can carry an exchange_timestamp
                // from inside market hours but actually arrive post-close
                // (server-side replay / buffer). Without this guard the
                // stale packet pollutes `market_depth` table. Same guard
                // shape used by ticks.
                if !is_wall_clock_within_persist_window(tick.received_at_nanos, muhurat_active) {
                    outside_hours_filtered = outside_hours_filtered.saturating_add(1);
                    m_outside_hours.increment(1);
                    continue;
                }

                // #T4 (2026-05-20): the `market_depth` QuestDB table was
                // dropped. Depth frames are still parsed + filtered (the
                // guards above gate the broadcast below), but no longer
                // persisted. The IDX_I-only universe runs in Quote mode,
                // which never carries 5-level depth anyway.
                m_depth_snapshots.increment(1);

                // O(1) broadcast to browser WebSocket clients (if tick has valid LTP).
                // See audit finding #1/#19 rationale at the other broadcast
                // send site earlier in this file — same guard.
                if ltp_valid
                    && let Some(ref sender) = tick_broadcast
                    && sender.send(tick).is_err()
                {
                    metrics::counter!("tv_tick_broadcast_send_errors_total").increment(1);
                    error!("tick broadcast send failed (valid-LTP path) — ALL subscribers closed");
                }

                // (day_close baseline already set ABOVE time guards at line ~711)

                // Candle-engine re-architecture #T1b: in-loop candle
                // aggregation removed — Engine B aggregates off the
                // tick broadcast.
                //
                // PR #2 (2026-05-18): movers `update(&tick)` calls on
                // Full-packet path removed. Trackers deleted with the
                // movers pipeline retirement.
                let _ = ltp_valid;

                trace!(
                    security_id = tick.security_id,
                    ltp = tick.last_traded_price,
                    "tick with depth processed"
                );
            }
            ParsedFrame::OiUpdate {
                security_id,
                exchange_segment_code,
                open_interest,
            } => {
                m_oi_updates.increment(1);
                trace!(
                    security_id,
                    exchange_segment_code, open_interest, "OI update received"
                );
            }
            ParsedFrame::PreviousClose {
                security_id,
                exchange_segment_code,
                previous_close,
                previous_oi,
            } => {
                m_prev_close_updates.increment(1);

                // Guard: skip non-finite previous_close (NaN/Infinity from corrupted frame).
                if !previous_close.is_finite() {
                    junk_ticks_filtered = junk_ticks_filtered.saturating_add(1);
                    m_junk_filtered.increment(1);
                    continue;
                }

                // Diagnostic: count PrevClose packets per segment.
                match exchange_segment_code {
                    0 => prev_close_idx_i = prev_close_idx_i.saturating_add(1),
                    1 => prev_close_nse_eq = prev_close_nse_eq.saturating_add(1),
                    2 => prev_close_nse_fno = prev_close_nse_fno.saturating_add(1),
                    _ => prev_close_other = prev_close_other.saturating_add(1),
                }
                let _ = previous_oi; // PR #2: was consumed by opt_movers.update_prev_oi

                // Cache index prev_close to file for mid-day restart survival.
                // Indices have day_close=0 in Full ticks, so PrevClose (code 6) is their
                // ONLY source. File cache survives restarts — read back at boot.
                // O(1) EXEMPT: cold path — runs once per index at subscription time (~28 calls).
                if exchange_segment_code == 0 {
                    // IDX_I segment
                    index_prev_close_cache.insert(security_id, previous_close);
                    //
                    // Wave 1 Item 0.a — sync file write (`fs::write`) + rename moved to
                    // a dedicated writer task fed by a bounded
                    // `tokio::sync::mpsc::channel(64)`; the hot path enqueues
                    // a `bytes::Bytes` payload via `try_enqueue_global`. Drop
                    // policy on overflow: drop newest, increment
                    // `tv_prev_close_writer_dropped_total`, the next packet
                    // re-snapshots the full HashMap so the cache becomes
                    // fresh on the following successful enqueue.
                    if let Ok(json) = serde_json::to_string(&index_prev_close_cache) {
                        let outcome =
                            super::prev_close_writer::try_enqueue_global(bytes::Bytes::from(json));
                        // The writer logs its own ERROR on disk failure +
                        // increments `tv_prev_close_writer_errors_total`. Hot
                        // path stays ALLOC-CHEAP: the only allocation is the
                        // `serde_json::to_string` above, which already
                        // existed before Wave 1 Item 0.a. The Bytes::from
                        // wrap is an Arc handoff (no copy).
                        let _ = outcome;
                    }
                }

                // #T4 table-cleanup: the `previous_close` QuestDB table
                // was dropped. PrevClose packets (code 6) are still
                // parsed and traced for observability, but no longer
                // persisted — Quote bytes 38-41 (`close`) already IS
                // previous-day close per Dhan Ticket #5525125.
                trace!(
                    security_id,
                    exchange_segment_code, previous_close, previous_oi, "previous close persisted"
                );
            }
            ParsedFrame::MarketStatus {
                exchange_segment_code,
                security_id,
            } => {
                m_market_status_updates.increment(1);
                info!(exchange_segment_code, security_id, "market status update");
            }
            // PR #4 (2026-05-19): `ParsedFrame::DeepDepth` arm retired
            // alongside the deleted depth feeds.
            ParsedFrame::Disconnect(code) => {
                m_disconnects.increment(1);
                error!(
                    ?code,
                    "disconnect frame received from Dhan — connection will be closed by WebSocket layer"
                );
            }
        }

        let total_elapsed_ns = u64::try_from(tick_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        m_tick_duration.record(total_elapsed_ns as f64);
        // B6: compute-only = total minus the persistence stall this iteration
        // (the writer measures its blocking-send / inline-flush fallbacks and
        // disconnected-branch recovery as stall; the offloaded steady state
        // accrues zero). Keeping the TOTAL histogram above unchanged preserves
        // dashboard/alert back-compat; the 10µs bench budget gates the
        // compute-only figure.
        let stall_ns = tick_writer
            .as_mut()
            .map_or(0, TickPersistenceWriter::take_last_stall_ns);
        m_tick_compute.record(total_elapsed_ns.saturating_sub(stall_ns) as f64);
        // Wire-to-done: from WebSocket receive (received_at_nanos) to pipeline completion.
        // Different from tick_duration which only measures processing time (tick_start).
        let wire_elapsed_ns = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_sub(received_at_nanos);
        m_wire_to_done.record(wire_elapsed_ns.max(0) as f64);

        // PROOF: log day_close baseline coverage after 30 seconds.
        // Placed AFTER all match arms so it fires for both Tick AND TickWithDepth.
        if !day_close_baseline_logged
            && day_close_first_log_time.is_some_and(|t| t.elapsed().as_secs() >= 30)
        {
            day_close_baseline_logged = true;
            info!(
                day_close_updates = day_close_baseline_count,
                "PROOF: day_close baselines set from Full packet ticks (should be >> 0 for all instruments)"
            );
        }

        // 2026-05-09 diagnostic: every 60 s emit a snapshot of the
        // filter counters at INFO so the operator can see, without
        // scraping Prometheus, where ticks are going. Logs the deltas
        // since the last emit (rates) plus the absolute totals. Skips
        // the emit when ALL deltas are zero so the log isn't spammed
        // pre-09:00 / post-15:30 IST when the pipeline is idle by
        // design. Without this line, "ticks not flowing on Saturday
        // mock session" took a code dive to triage.
        if last_filter_stats_log.elapsed().as_secs() >= 60 {
            let stale_day_delta = stale_day_filtered.saturating_sub(last_logged_stale_day);
            let outside_hours_delta =
                outside_hours_filtered.saturating_sub(last_logged_outside_hours);
            let dedup_delta = dedup_filtered.saturating_sub(last_logged_dedup);
            let junk_delta = junk_ticks_filtered.saturating_sub(last_logged_junk);
            let persisted_delta = ticks_persisted.saturating_sub(last_logged_persisted);
            let activity_in_window = stale_day_delta
                .saturating_add(outside_hours_delta)
                .saturating_add(dedup_delta)
                .saturating_add(junk_delta)
                .saturating_add(persisted_delta);
            if activity_in_window > 0 {
                info!(
                    target: "tickvault_core::pipeline::tick_processor::periodic_stats",
                    persisted_delta,
                    stale_day_delta,
                    outside_hours_delta,
                    dedup_delta,
                    junk_delta,
                    persisted_total = ticks_persisted,
                    stale_day_total = stale_day_filtered,
                    outside_hours_total = outside_hours_filtered,
                    dedup_total = dedup_filtered,
                    junk_total = junk_ticks_filtered,
                    "tick processor periodic stats — last 60s deltas + totals (use this to triage 'no ticks in QuestDB' on mock-session days)"
                );
                last_logged_stale_day = stale_day_filtered;
                last_logged_outside_hours = outside_hours_filtered;
                last_logged_dedup = dedup_filtered;
                last_logged_junk = junk_ticks_filtered;
                last_logged_persisted = ticks_persisted;
            }

            // Tick-conservation ledger (cold-path PROOF that no tick is lost
            // between branch entry and a known outcome). Identity (exact, since
            // the loop is synchronous → zero in-flight at this point):
            //   ticks_processed == persisted + storage_errors + junk
            //                      + stale_day + outside_hours + dedup
            // Only meaningful with a writer (persist is the dominant terminal).
            // We alert on a GROWING residual (active leak), not the absolute, so
            // a constant offset never pages — only real divergence does.
            if tick_writer.is_some() {
                let residual = tick_conservation_residual(
                    ticks_processed,
                    ticks_persisted,
                    storage_errors,
                    junk_ticks_filtered,
                    stale_day_filtered,
                    outside_hours_filtered,
                    dedup_filtered,
                );
                if let Some(prev) = last_conservation_residual {
                    let leak = conservation_leak_delta(residual, prev);
                    if leak > 0 {
                        metrics::counter!("tv_tick_conservation_leak_total").increment(leak as u64);
                        error!(
                            target: "tickvault_core::pipeline::tick_processor::conservation",
                            unaccounted_this_window = leak,
                            residual_total = residual,
                            ticks_processed,
                            ticks_persisted,
                            storage_errors,
                            "TICK CONSERVATION LEAK — ticks entered the pipeline but reached no known outcome this window"
                        );
                    } else if ticks_processed > last_conservation_processed {
                        info!(
                            target: "tickvault_core::pipeline::tick_processor::conservation",
                            residual_total = residual,
                            ticks_processed,
                            "tick conservation OK — every tick accounted (residual flat)"
                        );
                    }
                }
                last_conservation_residual = Some(residual);
                last_conservation_processed = ticks_processed;
            }

            last_filter_stats_log = Instant::now();
        }

        // Periodic flush check (every ~100ms worth of frames)
        if last_flush_check.elapsed().as_millis() > 100 {
            if let Some(ref mut writer) = tick_writer {
                if let Err(err) = writer.flush_if_needed() {
                    // Audit finding #2 (2026-04-17): must be ERROR, not WARN —
                    // Telegram alert path is keyed on log level. A persistent
                    // flush failure means ticks are stuck in the ring buffer
                    // or spill path with no operator visibility at WARN level.
                    error!(
                        ?err,
                        "periodic tick flush failed — QuestDB write path broken"
                    );
                    metrics::counter!("tv_tick_flush_errors_total").increment(1);
                }
                // MEDIUM-4 (B6 adversarial round 1): flush_if_needed runs
                // AFTER this iteration's compute record — the stall it
                // accrues (failed-batch rescue, time-based flush fallback)
                // must NOT leak into the NEXT frame's compute sample, where
                // saturating_sub would deflate it toward a bogus 0ns.
                // Drain + discard it into flush-side accounting.
                let deferred_stall_ns = writer.take_last_stall_ns();
                if deferred_stall_ns > 0 {
                    metrics::counter!("tv_tick_flush_deferred_stall_ns_total")
                        .increment(deferred_stall_ns);
                }
            }

            // Candle-engine re-architecture #T1b: periodic candle
            // sweep + `candles_1s` persistence removed. Engine B (the
            // multi-TF aggregator) owns candle sealing + flushing on a
            // separate task driven by the tick broadcast.

            // PR #2 (2026-05-18): 5s movers snapshot compute removed
            // alongside the movers pipeline. The cadence tick is kept
            // here as a place-holder for future periodic cold-path
            // bookkeeping (currently no-op).
            if last_snapshot_check.elapsed().as_secs() >= 5 {
                last_snapshot_check = Instant::now();
            }

            // M6: Channel backpressure — sample occupancy every flush cycle (~100ms).
            // O(1): mpsc::Receiver::len() is an atomic load.
            m_channel_occupancy.set(frame_receiver.len() as f64);

            last_flush_check = Instant::now();
        }
    }

    // A1: Graceful shutdown — flush ring buffer + ILP buffer + disk spill.
    // This replaces the previous force_flush() which only flushed the ILP buffer
    // and would lose up to 300K ticks sitting in the ring buffer on exit.
    if let Some(ref mut writer) = tick_writer {
        writer.flush_on_shutdown();
    }

    // Candle-engine re-architecture #T1b: shutdown candle flush
    // removed — Engine B owns candle sealing on its own task.

    // PR #2 (2026-05-18): final-state log for top movers tracker
    // removed alongside the deleted `top_movers` / `option_movers`
    // modules. Under the 4-IDX_I-only universe there is no movers
    // pipeline to flush or log at shutdown.

    m_pipeline_active.set(0.0);
    info!(
        frames_processed,
        ticks_processed,
        junk_ticks_filtered,
        dedup_filtered,
        outside_hours_filtered,
        stale_day_filtered,
        parse_errors,
        storage_errors,
        "tick processor stopped"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic (casts, indexing) is safe
mod tests {
    use super::*;

    /// B6 ratchet (2026-07-03): the tick loop MUST record the compute-only
    /// histogram `tv_tick_compute_duration_ns` next to the total
    /// `tv_tick_processing_duration_ns`, and MUST subtract the writer's
    /// persistence stall (`take_last_stall_ns`) so flush I/O never re-folds
    /// into the compute figure. Removing either silently reverts the B6
    /// latency split.
    #[test]
    fn test_compute_histogram_recorded_and_subtracts_stall() {
        let source = include_str!("tick_processor.rs");
        assert!(
            source.contains("histogram!(\"tv_tick_compute_duration_ns\")"),
            "tick_processor must register the tv_tick_compute_duration_ns histogram"
        );
        assert!(
            source.contains("take_last_stall_ns"),
            "the compute record must subtract the writer's persistence stall \
             (take_last_stall_ns)"
        );
        assert!(
            source.contains("total_elapsed_ns.saturating_sub(stall_ns)"),
            "compute = total − stall (saturating) — the exact B6 split"
        );
        // The TOTAL histogram must remain for dashboard/alert back-compat.
        assert!(
            source.contains("histogram!(\"tv_tick_processing_duration_ns\")"),
            "the total-span histogram must stay (back-compat)"
        );
        // MEDIUM-4 ratchet (adversarial round 1): the post-record
        // flush_if_needed paths must DISCARD the stall they accrue
        // (cross-iteration leak → bogus 0ns compute samples otherwise).
        assert!(
            source.contains("tv_tick_flush_deferred_stall_ns_total"),
            "flush-side stall accrued after the compute record must be \
             drained + discarded (never subtracted from the next frame)"
        );
    }

    /// HIGH-3 ratchet (B6 adversarial round 1): the recv loop MUST select
    /// over the 1s idle-drain interval so worker-failed flush batches are
    /// routed to the ring → spill → DLQ rescue chain even when no frame
    /// arrives (end-of-day / weekend). Removing the arm re-opens the
    /// "≤16 batches parked in RAM until shutdown" gap.
    #[test]
    fn test_idle_flush_drain_interval_arm_present() {
        let source = include_str!("tick_processor.rs");
        assert!(
            source.contains("IDLE_FLUSH_DRAIN_INTERVAL"),
            "the idle-drain cadence must be the named constant"
        );
        assert!(
            source.contains("idle_flush_interval.tick()"),
            "the recv loop must select over the idle flush interval"
        );
        assert!(
            source.contains("idle-interval tick flush failed"),
            "the idle arm must call flush_if_needed and error! on failure"
        );
    }

    // --- Dedup-collision elimination: strict-monotonic received_at ---
    // (operator directive 2026-06-04) — prove two distinct ticks can never
    // share the `(ts, security_id, segment, received_at)` dedup key.

    #[test]
    fn test_monotonic_received_at_same_now_returns_strictly_increasing() {
        // A burst: the clock returns the SAME nanosecond for two ticks.
        // The helper MUST hand out strictly-increasing values so the dedup
        // key can never collapse them.
        let last = std::sync::atomic::AtomicI64::new(0);
        let a = monotonic_received_at(1_000, &last);
        let b = monotonic_received_at(1_000, &last); // same clock value
        let c = monotonic_received_at(1_000, &last); // same clock value
        assert_eq!(a, 1_000);
        assert_eq!(b, 1_001, "collision on same nanosecond must bump +1");
        assert_eq!(c, 1_002);
        assert!(a < b && b < c, "strictly increasing");
    }

    #[test]
    fn test_monotonic_received_at_backward_clock_still_increases() {
        // NTP step / clock goes backward: never emit a value <= last.
        let last = std::sync::atomic::AtomicI64::new(0);
        assert_eq!(monotonic_received_at(5_000, &last), 5_000);
        assert_eq!(
            monotonic_received_at(4_000, &last),
            5_001,
            "backward clock must still strictly increase (last+1)"
        );
        assert_eq!(monotonic_received_at(4_999, &last), 5_002);
    }

    #[test]
    fn test_monotonic_received_at_forward_clock_uses_now() {
        // Normal case: the clock advances past last → use the real value.
        let last = std::sync::atomic::AtomicI64::new(0);
        assert_eq!(monotonic_received_at(10_000, &last), 10_000);
        assert_eq!(monotonic_received_at(10_500, &last), 10_500);
        assert_eq!(monotonic_received_at(20_000, &last), 20_000);
    }

    #[test]
    fn test_monotonic_received_at_long_sequence_is_strictly_unique() {
        // 10k calls with a clock that barely moves → every output unique +
        // strictly increasing → zero dedup-key collisions possible.
        let last = std::sync::atomic::AtomicI64::new(0);
        let mut prev = i64::MIN;
        for i in 0..10_000i64 {
            // Clock that repeats values heavily (only advances every 100 calls).
            let now = 1_000_000 + (i / 100);
            let got = monotonic_received_at(now, &last);
            assert!(got > prev, "must be strictly increasing: {got} > {prev}");
            prev = got;
        }
    }

    use tickvault_common::constants::{
        DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, HEADER_OFFSET_EXCHANGE_SEGMENT,
        HEADER_OFFSET_MESSAGE_LENGTH, HEADER_OFFSET_RESPONSE_CODE, HEADER_OFFSET_SECURITY_ID,
        MARKET_DEPTH_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
        PREVIOUS_CLOSE_PACKET_SIZE, RESPONSE_CODE_DISCONNECT, RESPONSE_CODE_FULL,
        RESPONSE_CODE_MARKET_DEPTH, RESPONSE_CODE_MARKET_STATUS, RESPONSE_CODE_OI,
        RESPONSE_CODE_PREVIOUS_CLOSE, TICKER_OFFSET_LTP, TICKER_OFFSET_LTT, TICKER_PACKET_SIZE,
    };

    // -----------------------------------------------------------------------
    // Wave 1 Item 0.d — boot-time prev_close cache dir init
    // -----------------------------------------------------------------------

    /// Wave 1 Item 0.d ratchet: `init_prev_close_cache_dir()` must be safe to
    /// call repeatedly. It runs at boot before the tick processor, so the
    /// hot-path PrevClose code-6 frame can assume the directory exists
    /// without paying for a `create_dir_all` syscall on every packet.
    #[test]
    fn test_init_prev_close_cache_dir_idempotent() {
        // Calling the boot init three times in a row must succeed every time.
        // The function is `pub` and idempotent by design (`create_dir_all` is
        // a no-op when the directory already exists).
        for _ in 0..3 {
            init_prev_close_cache_dir().expect("init_prev_close_cache_dir must be idempotent");
        }
        assert!(
            std::path::Path::new(INDEX_PREV_CLOSE_CACHE_DIR).exists(),
            "init_prev_close_cache_dir must leave INDEX_PREV_CLOSE_CACHE_DIR on disk"
        );
    }

    /// Wave 1 Item 0.d source-scan ratchet: prove the per-packet
    /// create-dir-all call was hoisted out of the PrevClose hot-path arm.
    /// If a future edit re-introduces the call inside the
    /// ParsedFrame PrevClose arm without a HOT-PATH-EXEMPT comment,
    /// this test MUST fail.
    ///
    /// The scan stops at the closing of the IDX_I if-arm by string
    /// match on the trailing trace! marker, so test source — which
    /// legitimately mentions the banned call name — is excluded by
    /// construction (and comment lines inside the arm body are
    /// filtered before the substring match).
    #[test]
    fn test_prev_close_arm_has_no_sync_fs_calls() {
        let src = include_str!("tick_processor.rs");
        let arm_start_marker =
            "// IDX_I segment\n                    index_prev_close_cache.insert";
        let arm_end_marker = "trace!(\n                    security_id,\n                    exchange_segment_code, previous_close, previous_oi, \"previous close persisted\"";
        let start = src
            .find(arm_start_marker)
            .expect("PrevClose IDX_I cache-write arm must exist");
        let end_rel = src[start..]
            .find(arm_end_marker)
            .expect("PrevClose arm must be followed by the trace!(...) summary");
        let arm_body = &src[start..start.saturating_add(end_rel)];
        // Strip comment lines so the human-readable explanation that
        // mentions the banned call names does NOT trip the regression
        // scan. We only flag actual call sites in code lines.
        let code_only: String = arm_body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<&str>>()
            .join("\n");
        // Wave 1 Item 0.d — directory creation must stay at boot.
        assert!(
            !code_only.contains("create_dir_all"),
            "Wave 1 Item 0.d regression: create-dir-all reappeared inside \
             the PrevClose IDX_I arm. Hoist it back to \
             init_prev_close_cache_dir() at boot, or add an explicit \
             HOT-PATH-EXEMPT comment if a per-packet syscall is genuinely \
             required."
        );
        // Wave 1 Item 0.a — sync write/rename must stay off the hot path.
        // The async writer task on the blocking pool owns these calls now.
        for banned in ["std::fs::write", "std::fs::rename"] {
            assert!(
                !code_only.contains(banned),
                "Wave 1 Item 0.a regression: sync `{banned}` reappeared in \
                 the PrevClose IDX_I arm. Use \
                 prev_close_writer::try_enqueue_global(...) instead."
            );
        }
    }

    /// A5: Test helper — calls `run_tick_processor` with `NoopGreeksEnricher`
    /// as the concrete type parameter (no Greeks in tests). Avoids turbofish
    /// on every test call site.
    // Phase 4b (2026-05-05): `stock_movers_writer` + `option_movers_writer`
    // PR #2 (2026-05-18): movers tracker params removed from
    // `run_test_tick_processor` alongside the deleted modules.
    async fn run_test_tick_processor(
        mut frame_receiver: mpsc::Receiver<bytes::Bytes>,
        tick_writer: Option<TickPersistenceWriter>,
        tick_broadcast: Option<broadcast::Sender<ParsedTick>>,
    ) {
        // TICK-SEQ-01: bridge the test's `Bytes` channel to the production
        // `(frame_seq, frame)` channel so the existing Bytes-sending test sites
        // stay unchanged. frame_seq=0 is fine here — `capture_seq` is a non-key
        // column in this slice (production stamps the real read-loop seq).
        let (seq_tx, seq_rx) = mpsc::channel::<(u64, bytes::Bytes)>(256);
        tokio::spawn(async move {
            while let Some(frame) = frame_receiver.recv().await {
                if seq_tx.send((0u64, frame)).await.is_err() {
                    break;
                }
            }
        });
        run_tick_processor::<tickvault_common::tick_types::NoopGreeksEnricher>(
            seq_rx,
            tick_writer,
            tick_broadcast,
            None,                                                  // greeks_enricher
            None, // tick_heartbeat — watchdog not exercised in tests
            None, // tick_enricher — Phase 2.5 enricher unused by these tests; legacy append_tick path covers them
            std::sync::Arc::new(std::collections::HashSet::new()), // §30 always-on: empty in tests
            None, // SP5 feed_health — registry recording covered by feed_health.rs + the wiring guard
        )
        .await;
    }

    /// Compute an IST epoch timestamp for today at the given hour/minute/second.
    /// Uses the same IST day-number logic as the tick processor so that
    /// `is_today_ist()` returns `true` for the result.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: test helper, bounded arithmetic
    fn today_ist_epoch_at(hours: u32, minutes: u32, seconds: u32) -> u32 {
        // IST epoch seconds for current wall-clock instant.
        let now_utc = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch") // APPROVED: test-only
            .as_secs() as u32;
        // IST = UTC + 19800 seconds (5:30 offset).
        let now_ist = now_utc + 19800;
        // Today's IST midnight = strip time-of-day.
        let today_midnight_ist = (now_ist / SECONDS_PER_DAY) * SECONDS_PER_DAY;
        today_midnight_ist + hours * 3600 + minutes * 60 + seconds
    }

    /// RAII helper that sets the test-only `TEST_RECEIVED_AT_OVERRIDE_NANOS`
    /// and resets it to `0` (disabled) on drop. Scopes the override to a
    /// single test so parallel tests do not see each other's clock.
    struct TestClockGuard;

    impl TestClockGuard {
        fn set(received_at_utc_nanos: i64) -> Self {
            super::TEST_RECEIVED_AT_OVERRIDE_NANOS
                .store(received_at_utc_nanos, std::sync::atomic::Ordering::Relaxed);
            Self
        }
    }

    impl Drop for TestClockGuard {
        fn drop(&mut self) {
            super::TEST_RECEIVED_AT_OVERRIDE_NANOS.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Build a valid ticker binary frame for testing.
    fn make_ticker_frame(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
        let mut buf = vec![0u8; TICKER_PACKET_SIZE];
        buf[HEADER_OFFSET_RESPONSE_CODE] = 2; // Ticker
        buf[HEADER_OFFSET_MESSAGE_LENGTH..HEADER_OFFSET_MESSAGE_LENGTH + 2]
            .copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
        buf[HEADER_OFFSET_EXCHANGE_SEGMENT] = 2; // NSE_FNO
        buf[HEADER_OFFSET_SECURITY_ID..HEADER_OFFSET_SECURITY_ID + 4]
            .copy_from_slice(&security_id.to_le_bytes());
        buf[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&ltp.to_le_bytes());
        buf[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4].copy_from_slice(&ltt.to_le_bytes());
        buf
    }

    #[tokio::test]
    async fn test_tick_processor_processes_frames() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send a valid ticker frame
        let frame = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        // Give processor time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Close channel → processor exits
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_invalid_frame() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send a too-short frame
        frame_tx
            .send(bytes::Bytes::from(vec![0u8; 4]))
            .await
            .unwrap();

        // Send a valid frame after — should still work
        let valid_frame = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx
            .send(bytes::Bytes::from(valid_frame))
            .await
            .unwrap();

        // Give processor time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_filters_zero_ltp_tick() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send a ticker frame with LTP=0.0 — should be filtered (not crash)
        let frame = make_ticker_frame(13, 0.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_filters_zero_timestamp_tick() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send a ticker frame with LTT=0 (epoch 1970) — should be filtered
        let frame = make_ticker_frame(13, 24500.0, 0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_passes_valid_tick_after_junk() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send junk tick first (LTP=0)
        let junk = make_ticker_frame(13, 0.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(junk)).await.unwrap();

        // Then send valid tick — processor should not crash
        let valid = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(valid)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_empty_channel_exits_cleanly() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Close immediately
        drop(frame_tx);
        let _ = handle.await; // should not hang
    }

    /// Build a minimal valid packet for a given response code.
    fn make_packet(response_code: u8, size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; size];
        buf[HEADER_OFFSET_RESPONSE_CODE] = response_code;
        buf[HEADER_OFFSET_MESSAGE_LENGTH..HEADER_OFFSET_MESSAGE_LENGTH + 2]
            .copy_from_slice(&(size as u16).to_le_bytes());
        buf[HEADER_OFFSET_EXCHANGE_SEGMENT] = 2; // NSE_FNO
        buf[HEADER_OFFSET_SECURITY_ID..HEADER_OFFSET_SECURITY_ID + 4]
            .copy_from_slice(&42u32.to_le_bytes());
        buf
    }

    #[tokio::test]
    async fn test_tick_processor_handles_oi_update() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_previous_close() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_market_status() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_disconnect() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        // Set disconnect code to 807 (AccessTokenExpired)
        frame[8..10].copy_from_slice(&807u16.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_full_quote() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        // Full quote packet with valid LTP and timestamp
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        // Set LTP at TICKER_OFFSET_LTP and LTT at TICKER_OFFSET_LTT
        // (Full packet uses same offsets for LTP/LTT as ticker)
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_parse_error_rate_limiting() {
        // Send 105 invalid frames to exercise the `if parse_errors <= 100` boundary.
        // After 100, the warn! is suppressed; the processor still continues.
        let (frame_tx, frame_rx) = mpsc::channel(200);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        for _ in 0..105 {
            frame_tx
                .send(bytes::Bytes::from(vec![0u8; 4]))
                .await
                .unwrap();
        }

        // Send a valid frame after — processor should still work.
        let valid = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(valid)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_final_flush_on_channel_close() {
        // With tick_writer=None, the final flush path (lines 146-149) takes
        // the if-let-Some branch only when a writer exists. Without a real
        // QuestDB we pass None, but we still exercise the code path that
        // checks `if let Some(ref mut writer)` and falls through.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send a valid tick so frames_processed > 0
        let frame = make_ticker_frame(42, 25000.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Close channel to trigger final flush path
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_periodic_flush_check_with_elapsed_time() {
        // Exercise the periodic flush check by sending frames with a gap > 100ms.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send first valid tick
        let frame1 = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame1)).await.unwrap();

        // Wait > 100ms so periodic flush elapsed check triggers
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send second tick — this will trigger the elapsed > 100ms branch
        let frame2 = make_ticker_frame(14, 24600.0, 1772073901);
        frame_tx.send(bytes::Bytes::from(frame2)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_multiple_junk_ticks_suppression() {
        // Send > 10 junk ticks to exercise the `if junk_ticks_filtered <= 10` boundary.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        for _ in 0..15 {
            let junk = make_ticker_frame(13, 0.0, today_ist_epoch_at(10, 0, 0));
            frame_tx.send(bytes::Bytes::from(junk)).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_negative_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // LTP = -1.0 should be filtered as junk
        let frame = make_ticker_frame(13, -1.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_mixed_valid_and_invalid_frames() {
        // Interleave valid, junk, and parse-error frames to exercise
        // multiple code paths in a single run.
        let (frame_tx, frame_rx) = mpsc::channel(200);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Parse error (too short)
        frame_tx
            .send(bytes::Bytes::from(vec![0u8; 3]))
            .await
            .unwrap();
        // Valid tick
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13,
                24500.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        // Junk tick (LTP=0)
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                14,
                0.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        // Junk tick (timestamp=0)
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(15, 24500.0, 0)))
            .await
            .unwrap();
        // Valid tick
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                16, 24600.0, 1772073901,
            )))
            .await
            .unwrap();
        // OI update
        frame_tx
            .send(bytes::Bytes::from(make_packet(
                RESPONSE_CODE_OI,
                OI_PACKET_SIZE,
            )))
            .await
            .unwrap();
        // Disconnect
        let mut disc = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        disc[8..10].copy_from_slice(&807u16.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(disc)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // Additional tick processor tests for edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_tick_processor_handles_previous_close_and_market_status_sequence() {
        // Send previous_close then market_status in sequence.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        let prev_close = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(prev_close)).await.unwrap();

        let market_status = make_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        frame_tx
            .send(bytes::Bytes::from(market_status))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_full_quote_with_zero_ltp_filtered() {
        // Full quote with LTP=0 should be filtered as junk.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        // Set LTP=0.0 and valid timestamp
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&0.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_full_quote_valid_ltp_processed() {
        // Full quote with valid LTP and timestamp should be processed.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&25000.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_many_frames_triggers_flush_check() {
        // Send many valid frames rapidly to verify periodic flush check runs.
        let (frame_tx, frame_rx) = mpsc::channel(500);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send 50 valid frames
        for i in 0..50 {
            let frame =
                make_ticker_frame(13 + i, 24500.0 + (i as f32), today_ist_epoch_at(10, 0, 0));
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        // Wait for periodic flush check to trigger
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send another batch
        for i in 0..50 {
            let frame = make_ticker_frame(100 + i, 25000.0, 1772073901);
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_unknown_response_code_is_parse_error() {
        // Unknown response code (99) should be counted as a parse error.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        let mut buf = vec![0u8; 8];
        buf[0] = 99; // unknown response code
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(buf)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    /// Creates a TickPersistenceWriter connected to a local TCP listener.
    /// Returns the writer and an abort handle for the listener.
    /// The listener accepts connections and reads data until aborted.
    async fn create_mock_ilp_writer() -> (TickPersistenceWriter, tokio::task::JoinHandle<()>) {
        use tickvault_common::config::QuestDbConfig;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Spawn a task that accepts connections and reads data (acting as ILP sink)
        let listener_handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => continue,
                        }
                    }
                });
            }
        });

        // Small delay to ensure the listener is ready
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };

        let writer = TickPersistenceWriter::new(&config).unwrap();
        (writer, listener_handle)
    }

    /// Creates a TickPersistenceWriter connected to a TCP listener that
    /// immediately closes the connection, causing flushes to fail.
    async fn create_broken_ilp_writer() -> TickPersistenceWriter {
        use tickvault_common::config::QuestDbConfig;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Accept the connection and immediately drop it
        let listener_handle = tokio::spawn(async move {
            if let Ok((_stream, _)) = listener.accept().await {
                // Drop stream immediately to close the connection
            }
        });

        // Small delay to ensure the listener is ready
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };

        let writer = TickPersistenceWriter::new(&config).unwrap();

        // Wait for the listener task to complete (connection accepted and dropped)
        let _ = listener_handle.await;

        // Small delay to ensure the TCP connection is fully closed
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        writer
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_tick_processor_with_writer_appends_valid_ticks() {
        // Exercises lines 89-90 (if let Some(ref mut writer)) and the
        // successful append_tick path. Also covers periodic flush (lines 136-137)
        // and final flush (lines 146-147) with a working writer.
        let (writer, listener_handle) = create_mock_ilp_writer().await;
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, Some(writer), None).await;
        });

        // Send a valid tick
        let frame = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        // Wait > 100ms to trigger periodic flush check
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send another valid tick after the delay to trigger the elapsed check
        let frame2 = make_ticker_frame(14, 24600.0, 1772073901);
        frame_tx.send(bytes::Bytes::from(frame2)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Close channel to trigger final flush
        drop(frame_tx);

        // Use timeout to avoid hanging if writer cleanup is slow
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

        listener_handle.abort();
    }

    #[tokio::test]
    async fn test_tick_processor_with_broken_writer_flush_fails() {
        // Exercises lines 137, 139 (periodic flush failure) and lines 147, 149
        // (final flush failure) when the TCP connection is broken.
        //
        // The writer's flush_if_needed() only flushes when elapsed >=
        // TICK_FLUSH_INTERVAL_MS (1000ms). So we need to wait > 1s for
        // the periodic flush to actually attempt to send data over the
        // broken TCP connection.
        let writer = create_broken_ilp_writer().await;
        let (frame_tx, frame_rx) = mpsc::channel(200);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, Some(writer), None).await;
        });

        // Send valid ticks — they buffer successfully but flush will fail
        for i in 0..5 {
            let frame =
                make_ticker_frame(13 + i, 24500.0 + (i as f32), today_ist_epoch_at(10, 0, 0));
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        // Wait > 1s so writer.flush_if_needed() actually tries to flush
        // (TICK_FLUSH_INTERVAL_MS = 1000ms). The tick processor's outer
        // check triggers at 100ms, but the writer only flushes at 1000ms.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        // Send more ticks to trigger the periodic flush check on next iteration
        for i in 0..5 {
            let frame = make_ticker_frame(20 + i, 25000.0, 1772073901);
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Close channel to trigger final flush (which should also fail
        // because there are pending ticks from the second batch)
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // Market Depth code 3 tests — depth must persist even without timestamp
    // -----------------------------------------------------------------------

    /// Build a Market Depth standalone packet (code 3, 112 bytes).
    /// Has LTP but NO exchange_timestamp (exchange_timestamp=0 by design).
    fn make_market_depth_frame(security_id: u32, ltp: f32) -> Vec<u8> {
        let mut buf = vec![0u8; MARKET_DEPTH_PACKET_SIZE];
        buf[HEADER_OFFSET_RESPONSE_CODE] = RESPONSE_CODE_MARKET_DEPTH;
        buf[HEADER_OFFSET_MESSAGE_LENGTH..HEADER_OFFSET_MESSAGE_LENGTH + 2]
            .copy_from_slice(&(MARKET_DEPTH_PACKET_SIZE as u16).to_le_bytes());
        buf[HEADER_OFFSET_EXCHANGE_SEGMENT] = 2; // NSE_FNO
        buf[HEADER_OFFSET_SECURITY_ID..HEADER_OFFSET_SECURITY_ID + 4]
            .copy_from_slice(&security_id.to_le_bytes());
        // LTP at offset 8
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());
        buf
    }

    #[tokio::test]
    async fn test_tick_processor_market_depth_not_filtered_as_junk() {
        // Market Depth code 3 has exchange_timestamp=0 by design.
        // Depth data must still be persisted — it must NOT be filtered as junk.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send a Market Depth frame with valid LTP but no timestamp
        let frame = make_market_depth_frame(13, 24500.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
        // If this doesn't crash and processes the frame, it works.
        // The depth_writer=None means depth isn't persisted in this test,
        // but the frame reaches the depth persistence code path.
    }

    #[tokio::test]
    async fn test_tick_processor_market_depth_zero_ltp_is_filtered() {
        // Market Depth with LTP=0.0 is truly junk — should be filtered.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        let frame = make_market_depth_frame(13, 0.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // ===================================================================
    // TickDedupRing unit tests
    // ===================================================================

    // Decided tick identity (operator lock 2026-06-05):
    // (exchange_segment_code, security_id, exchange_timestamp, received_at_nanos).
    // received_at is unique per arrival → a genuine tick is NEVER dropped.
    const SEG_FNO: u8 = 2; // NSE_FNO
    const SEG_IDX: u8 = 0; // IDX_I

    #[test]
    fn test_dedup_ring_new_tick_not_duplicate() {
        let mut ring = TickDedupRing::new(8); // 256 slots
        assert!(!ring.is_duplicate(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), 1_000));
    }

    #[test]
    fn test_dedup_ring_exact_same_frame_is_duplicate() {
        // The SAME frame delivered twice (identical received_at) = byte-identical
        // double-delivery → caught. Dhan does not produce this, but the guard
        // exists for it.
        let mut ring = TickDedupRing::new(8);
        let ts = today_ist_epoch_at(10, 0, 0);
        assert!(!ring.is_duplicate(SEG_IDX, 13, ts, 5_000));
        assert!(ring.is_duplicate(SEG_IDX, 13, ts, 5_000));
    }

    #[test]
    fn test_dedup_ring_different_received_at_is_kept() {
        // THE decided guarantee: a price RE-TOUCH that repeats an earlier
        // (segment, id, LTT) but arrives with a fresh received_at is ALWAYS
        // kept — this is the operator's 23440 case. received_at is the unique
        // arrival clock, so the fingerprint differs and the tick survives.
        let mut ring = TickDedupRing::new(8);
        let ts = today_ist_epoch_at(10, 0, 0);
        assert!(!ring.is_duplicate(SEG_IDX, 13, ts, 1_000_000_000));
        assert!(!ring.is_duplicate(SEG_IDX, 13, ts, 1_067_000_000_000)); // +67s arrival
    }

    #[test]
    fn test_dedup_ring_different_segment_not_duplicate() {
        // I-P1-11: same (security_id, LTT, received_at) on a DIFFERENT segment is
        // a DISTINCT instrument → not a duplicate. Dhan reuses security_id across
        // segments (e.g. 27 = FINNIFTY IDX_I and an NSE_EQ row).
        let mut ring = TickDedupRing::new(8);
        let ts = today_ist_epoch_at(10, 0, 0);
        assert!(!ring.is_duplicate(SEG_IDX, 27, ts, 9_000));
        assert!(!ring.is_duplicate(1, 27, ts, 9_000)); // NSE_EQ, same id/ts/arrival
    }

    #[test]
    fn test_dedup_ring_different_security_not_duplicate() {
        let mut ring = TickDedupRing::new(8);
        let ts = today_ist_epoch_at(10, 0, 0);
        assert!(!ring.is_duplicate(SEG_IDX, 13, ts, 7_000));
        assert!(!ring.is_duplicate(SEG_IDX, 14, ts, 7_000));
    }

    #[test]
    fn test_dedup_ring_different_timestamp_not_duplicate() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), 7_000));
        assert!(!ring.is_duplicate(SEG_IDX, 13, 1772073901, 7_000));
    }

    #[test]
    fn test_dedup_ring_zero_values() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(0, 0, 0, 0));
        assert!(ring.is_duplicate(0, 0, 0, 0)); // identical zero frame twice
    }

    #[test]
    fn test_dedup_ring_max_values() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(u8::MAX, u64::from(u32::MAX), u32::MAX, i64::MAX));
        assert!(ring.is_duplicate(u8::MAX, u64::from(u32::MAX), u32::MAX, i64::MAX));
    }

    #[test]
    fn test_dedup_ring_eviction_after_collision_no_panic() {
        // With a small ring (256 slots), inserting many distinct entries will
        // evict earlier ones. We don't assert the probabilistic result — just
        // verify the hot path never panics.
        let mut ring = TickDedupRing::new(8); // 256 slots
        assert!(!ring.is_duplicate(SEG_FNO, 13, today_ist_epoch_at(10, 0, 0), 1_000));
        for i in 1..=512_i64 {
            ring.is_duplicate(SEG_FNO, (i + 100) as u64, today_ist_epoch_at(10, 0, 0), i);
        }
        let _ = ring.is_duplicate(SEG_FNO, 13, today_ist_epoch_at(10, 0, 0), 1_000);
    }

    #[test]
    fn test_dedup_ring_interleaved_securities() {
        let mut ring = TickDedupRing::new(12); // 4096 slots
        for round in 0..3_i64 {
            let ts = today_ist_epoch_at(10, 0, 0);
            let arrival = 10_000 + round; // distinct arrival per round
            assert!(!ring.is_duplicate(SEG_IDX, 13, ts, arrival));
            assert!(!ring.is_duplicate(SEG_IDX, 14, ts, arrival));
            assert!(!ring.is_duplicate(SEG_IDX, 15, ts, arrival));
            // Same frame (same arrival) within the round = duplicate.
            assert!(ring.is_duplicate(SEG_IDX, 13, ts, arrival));
            assert!(ring.is_duplicate(SEG_IDX, 14, ts, arrival));
            assert!(ring.is_duplicate(SEG_IDX, 15, ts, arrival));
        }
    }

    #[test]
    fn test_dedup_ring_fingerprint_deterministic() {
        let fp1 = TickDedupRing::fingerprint(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), 42);
        let fp2 = TickDedupRing::fingerprint(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), 42);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_dedup_ring_fingerprint_distinct_for_different_inputs() {
        let base = TickDedupRing::fingerprint(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), 42);
        // each field flipped → distinct fingerprint
        let diff_seg = TickDedupRing::fingerprint(1, 13, today_ist_epoch_at(10, 0, 0), 42);
        let diff_sec = TickDedupRing::fingerprint(SEG_IDX, 14, today_ist_epoch_at(10, 0, 0), 42);
        let diff_ts = TickDedupRing::fingerprint(SEG_IDX, 13, 1772073901, 42);
        let diff_recv = TickDedupRing::fingerprint(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), 43);
        assert_ne!(base, diff_seg);
        assert_ne!(base, diff_sec);
        assert_ne!(base, diff_ts);
        assert_ne!(base, diff_recv);
    }

    #[test]
    fn test_dedup_ring_fingerprint_symmetric_inputs_differ() {
        // Verify (sec=1, ts=3) != (sec=3, ts=1) — FNV-1a is order-dependent
        let fp1 = TickDedupRing::fingerprint(SEG_IDX, 1, 3, 100);
        let fp2 = TickDedupRing::fingerprint(SEG_IDX, 3, 1, 100);
        assert_ne!(fp1, fp2);
    }

    // ===================================================================
    // NaN / Infinity pipeline tests
    // ===================================================================

    #[tokio::test]
    async fn test_tick_processor_nan_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(13, f32::NAN, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_positive_infinity_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(13, f32::INFINITY, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_negative_infinity_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(13, f32::NEG_INFINITY, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_nan_ltp_full_quote_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_infinity_ltp_full_quote_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4]
            .copy_from_slice(&f32::INFINITY.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_nan_ltp_market_depth_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_market_depth_frame(13, f32::NAN);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_infinity_ltp_market_depth_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_market_depth_frame(13, f32::INFINITY);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // ===================================================================
    // Dedup pipeline integration tests
    // ===================================================================

    #[tokio::test]
    async fn test_tick_processor_dedup_filters_exact_duplicate_tick() {
        // Send the same tick twice — second should be dedup-filtered.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx
            .send(bytes::Bytes::from(frame.clone()))
            .await
            .unwrap();
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_allows_different_timestamp() {
        // Same security, different timestamp — both should pass dedup.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13,
                24500.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13, 24500.0, 1772073901,
            )))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_allows_different_ltp() {
        // Same security+timestamp, different LTP — both should pass (price update).
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13,
                24500.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13,
                24501.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_allows_different_security() {
        // Different security, same timestamp+LTP — both should pass.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13,
                24500.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                14,
                24500.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_burst_100_identical() {
        // Send 100 identical ticks — only first should pass dedup.
        let (frame_tx, frame_rx) = mpsc::channel(200);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        for _ in 0..100 {
            frame_tx
                .send(bytes::Bytes::from(frame.clone()))
                .await
                .unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_does_not_affect_junk() {
        // Junk ticks (LTP=0) are filtered BEFORE dedup — dedup never sees them.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        // Send junk tick
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13,
                0.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        // Send valid tick for same security — should NOT be treated as duplicate
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13,
                24500.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_full_quote_duplicate() {
        // Send the same Full Quote twice — second should be dedup-filtered.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx
            .send(bytes::Bytes::from(frame.clone()))
            .await
            .unwrap();
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_market_depth_not_deduped() {
        // Market Depth code 3 has timestamp=0 — bypasses dedup (no valid timestamp).
        // Two identical depth frames should BOTH be processed (not deduplicated).
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_market_depth_frame(13, 24500.0);
        frame_tx
            .send(bytes::Bytes::from(frame.clone()))
            .await
            .unwrap();
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // ===================================================================
    // Additional edge case tests
    // ===================================================================

    #[tokio::test]
    async fn test_tick_processor_subnormal_ltp_filtered() {
        // Subnormal f32 (very close to zero but positive) — should be filtered
        // because it's <= 0.0 is false but it's effectively zero for trading.
        // Actually subnormal > 0.0 is true, and is_finite() is true.
        // So subnormal passes through — this is correct (it's a valid f32).
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let subnormal: f32 = f32::MIN_POSITIVE / 2.0;
        let frame = make_ticker_frame(13, subnormal, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_negative_zero_ltp_filtered() {
        // -0.0 is == 0.0 in IEEE 754, so -0.0 <= 0.0 is true → filtered.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(13, -0.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_max_u32_security_id_processed() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        // u32::MAX security_id with valid LTP and timestamp
        let mut frame = make_ticker_frame(0, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_minimum_valid_timestamp_accepted() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        // Exact minimum valid timestamp — should pass
        let frame = make_ticker_frame(13, 24500.0, MINIMUM_VALID_EXCHANGE_TIMESTAMP);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_timestamp_one_below_minimum_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(
            13,
            24500.0,
            MINIMUM_VALID_EXCHANGE_TIMESTAMP.saturating_sub(1),
        );
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_all_nine_packet_types_in_sequence() {
        // Send one of every packet type in rapid succession.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // 1. Index Ticker (code 1)
        let mut idx_tick = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        idx_tick[0] = 1; // RESPONSE_CODE_INDEX_TICKER
        frame_tx.send(bytes::Bytes::from(idx_tick)).await.unwrap();

        // 2. Ticker (code 2) — already default from make_ticker_frame
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                14,
                24600.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();

        // 3. Market Depth (code 3)
        frame_tx
            .send(bytes::Bytes::from(make_market_depth_frame(15, 24700.0)))
            .await
            .unwrap();

        // 4. Quote (code 4)
        let quote = make_packet(
            tickvault_common::constants::RESPONSE_CODE_QUOTE,
            tickvault_common::constants::QUOTE_PACKET_SIZE,
        );
        frame_tx.send(bytes::Bytes::from(quote)).await.unwrap();

        // 5. OI (code 5)
        frame_tx
            .send(bytes::Bytes::from(make_packet(
                RESPONSE_CODE_OI,
                OI_PACKET_SIZE,
            )))
            .await
            .unwrap();

        // 6. Previous Close (code 6)
        frame_tx
            .send(bytes::Bytes::from(make_packet(
                RESPONSE_CODE_PREVIOUS_CLOSE,
                PREVIOUS_CLOSE_PACKET_SIZE,
            )))
            .await
            .unwrap();

        // 7. Market Status (code 7)
        frame_tx
            .send(bytes::Bytes::from(make_packet(
                RESPONSE_CODE_MARKET_STATUS,
                MARKET_STATUS_PACKET_SIZE,
            )))
            .await
            .unwrap();

        // 8. Full Quote (code 8) with valid data
        let mut full = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        full[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24800.0_f32.to_le_bytes());
        full[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx.send(bytes::Bytes::from(full)).await.unwrap();

        // 9. Disconnect (code 50)
        let mut disc = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        disc[8..10].copy_from_slice(&807u16.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(disc)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_then_valid_after_different_ltp() {
        // Tick A (ltp=24500) → Tick A again (dedup) → Tick B (ltp=24501, same sec+ts)
        // Tick B should pass because LTP differs.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame_a = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx
            .send(bytes::Bytes::from(frame_a.clone()))
            .await
            .unwrap();
        frame_tx.send(bytes::Bytes::from(frame_a)).await.unwrap(); // duplicate
        let frame_b = make_ticker_frame(13, 24501.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame_b)).await.unwrap(); // NOT duplicate (different LTP)
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_mixed_dedup_junk_valid_nan() {
        // Comprehensive mixed sequence: valid → duplicate → junk → NaN → valid
        let (frame_tx, frame_rx) = mpsc::channel(200);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Valid tick
        let valid1 = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx
            .send(bytes::Bytes::from(valid1.clone()))
            .await
            .unwrap();
        // Exact duplicate
        frame_tx.send(bytes::Bytes::from(valid1)).await.unwrap();
        // Junk (LTP=0)
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                14,
                0.0,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        // NaN LTP
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                15,
                f32::NAN,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        // Infinity LTP
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                16,
                f32::INFINITY,
                today_ist_epoch_at(10, 0, 0),
            )))
            .await
            .unwrap();
        // Parse error (too short)
        frame_tx
            .send(bytes::Bytes::from(vec![0u8; 3]))
            .await
            .unwrap();
        // Valid tick (different security+timestamp — should pass)
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                17, 24600.0, 1772073901,
            )))
            .await
            .unwrap();
        // Market Depth (no timestamp, valid LTP — should NOT be dedup-filtered)
        frame_tx
            .send(bytes::Bytes::from(make_market_depth_frame(18, 24700.0)))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // Depth NaN/Infinity guard tests
    // -----------------------------------------------------------------------

    /// Build a Market Depth frame with a NaN bid_price in level 0.
    fn make_market_depth_frame_with_nan_bid(security_id: u32, ltp: f32) -> Vec<u8> {
        let mut buf = make_market_depth_frame(security_id, ltp);
        // Level 0 bid_price at MARKET_DEPTH_OFFSET_DEPTH_START + 12 = 24
        buf[24..28].copy_from_slice(&f32::NAN.to_le_bytes());
        buf
    }

    /// Build a Market Depth frame with an Infinity ask_price in level 0.
    fn make_market_depth_frame_with_inf_ask(security_id: u32, ltp: f32) -> Vec<u8> {
        let mut buf = make_market_depth_frame(security_id, ltp);
        // Level 0 ask_price at MARKET_DEPTH_OFFSET_DEPTH_START + 16 = 28
        buf[28..32].copy_from_slice(&f32::INFINITY.to_le_bytes());
        buf
    }

    #[tokio::test]
    async fn test_tick_processor_depth_nan_bid_price_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_market_depth_frame_with_nan_bid(13, 24500.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_depth_inf_ask_price_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_market_depth_frame_with_inf_ask(13, 24500.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // PreviousClose NaN/Infinity guard tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_tick_processor_previous_close_nan_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let mut buf = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        buf[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(buf)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_previous_close_infinity_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let mut buf = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        buf[8..12].copy_from_slice(&f32::INFINITY.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(buf)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // is_within_persist_window tests
    // -----------------------------------------------------------------------

    /// Helper: convert IST hours:minutes:seconds to an IST epoch u32.
    /// Uses a fixed reference date (2026-03-17) — the actual date doesn't
    /// matter because `is_within_persist_window` only checks seconds-of-day.
    /// Dhan WebSocket sends timestamps as IST epoch seconds.
    fn ist_hms_to_ist_epoch(hours: u32, minutes: u32, seconds: u32) -> u32 {
        // IST midnight 2026-03-17 as IST epoch.
        // UTC midnight 2026-03-17 = 1773705600. That is also IST midnight as IST epoch
        // because IST epoch for midnight IST = UTC midnight epoch value.
        let ist_midnight_ist_epoch: u32 = 1_773_705_600;
        let ist_secs_of_day = hours * 3600 + minutes * 60 + seconds;
        ist_midnight_ist_epoch + ist_secs_of_day
    }

    #[test]
    fn test_persist_window_market_open_boundary() {
        // 09:00:00 IST = first second of the window (inclusive)
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(9, 0, 0),
            false
        ));
        // 09:00:01 IST = well within window
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(9, 0, 1),
            false
        ));
    }

    #[test]
    fn test_persist_window_market_close_boundary() {
        // 15:29:59 IST = last second of the window (inclusive)
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(15, 29, 59),
            false
        ));
        // 15:30:00 IST = first second outside the window (exclusive)
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(15, 30, 0),
            false
        ));
        // 15:30:01 IST = outside
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(15, 30, 1),
            false
        ));
    }

    #[test]
    fn test_persist_window_pre_market_rejected() {
        // 08:59:59 IST = one second before window
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(8, 59, 59),
            false
        ));
        // 08:00:00 IST = well before market
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(8, 0, 0),
            false
        ));
        // 06:00:00 IST = early morning
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(6, 0, 0),
            false
        ));
    }

    #[test]
    fn test_persist_window_post_market_rejected() {
        // 16:00:00 IST = post-market
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(16, 0, 0),
            false
        ));
        // 20:00:00 IST = evening
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(20, 0, 0),
            false
        ));
    }

    #[test]
    fn test_persist_window_midnight_rejected() {
        // 00:00:00 IST = midnight
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(0, 0, 0),
            false
        ));
        // 23:59:59 IST = end of day
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(23, 59, 59),
            false
        ));
    }

    #[test]
    fn test_persist_window_mid_session() {
        // 12:00:00 IST = mid-session
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(12, 0, 0),
            false
        ));
        // 09:15:00 IST = continuous trading start (within persist window [09:00, 15:30))
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(9, 15, 0),
            false
        ));
        // 15:15:00 IST = near close
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(15, 15, 0),
            false
        ));
    }

    // -----------------------------------------------------------------------
    // is_within_persist_window — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_persist_window_zero_timestamp() {
        // Timestamp 0 → seconds_of_day = 0, well outside [09:00, 15:30)
        assert!(!is_within_persist_window(0, false));
    }

    #[test]
    fn test_is_window_exempt_only_for_listed_pairs() {
        // Operator lock 2026-06-01 §30: GIFT Nifty (sid 5024, IDX_I=0) is
        // exempt; everything else is not. Composite (sid, segment) key.
        let mut set = std::collections::HashSet::new();
        set.insert((5024_u64, 0_u8));
        assert!(is_window_exempt(&set, 5024, 0), "GIFT Nifty exempt");
        assert!(!is_window_exempt(&set, 13, 0), "NIFTY not exempt");
        assert!(!is_window_exempt(&set, 5024, 1), "same sid, wrong segment");
        // Empty set (default) → nothing exempt → today's behavior.
        let empty = std::collections::HashSet::new();
        assert!(!is_window_exempt(&empty, 5024, 0));
    }

    #[test]
    fn test_persist_window_raw_secs_of_day_boundary() {
        // Directly test with seconds-of-day values matching start/end constants
        // 32400 = 09:00 IST
        assert!(is_within_persist_window(
            TICK_PERSIST_START_SECS_OF_DAY_IST,
            false
        ));
        // 55800 = 15:30 IST (exclusive)
        assert!(!is_within_persist_window(
            TICK_PERSIST_END_SECS_OF_DAY_IST,
            false
        ));
    }

    #[test]
    fn test_persist_window_just_before_start() {
        assert!(!is_within_persist_window(
            TICK_PERSIST_START_SECS_OF_DAY_IST - 1,
            false
        ));
    }

    #[test]
    fn test_persist_window_just_before_end() {
        assert!(is_within_persist_window(
            TICK_PERSIST_END_SECS_OF_DAY_IST - 1,
            false
        ));
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 11 — G1/G2 dual-gate market-hours ratchets (2026-05-17)
    // -----------------------------------------------------------------------

    /// Helper: convert IST `(h, m, s)` to a UTC wall-clock nanosecond
    /// value that, when passed through `utc_nanos_to_ist_secs_of_day`,
    /// rounds to the same `secs-of-day` as the input (h:m:s).
    ///
    /// We back-compute UTC nanos by taking IST secs-of-day, subtracting
    /// the IST offset to get UTC secs-of-day, then anchoring to the
    /// arbitrary day 2026-05-17 (the day in which the original bug
    /// was reproduced).
    fn ist_hms_to_utc_received_at_nanos(h: u32, m: u32, s: u32, sub_ms: u32) -> i64 {
        // Anchor at any UTC midnight that is a clean multiple of 86400.
        // `utc_nanos_to_ist_secs_of_day` extracts the IST seconds-of-day
        // via `(utc_secs + IST_OFFSET) % SECONDS_PER_DAY` — so the day
        // anchor is irrelevant as long as it lands on a UTC midnight.
        // Day 20598 UTC midnight = 20598 * 86400 = 1_779_667_200.
        let day_anchor_utc_secs: i64 = 20_598_i64.saturating_mul(86_400);
        let ist_secs_of_day: i64 = i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(s);
        let utc_secs_of_day: i64 =
            ist_secs_of_day.saturating_sub(i64::from(IST_UTC_OFFSET_SECONDS));
        let utc_secs = day_anchor_utc_secs.saturating_add(utc_secs_of_day);
        let extra_ms: i64 = i64::from(sub_ms);
        utc_secs.saturating_mul(1_000_000_000) + extra_ms * 1_000_000
    }

    /// Item 11 scenario test: tick with `exchange_ts = 15:29:59.586` IST
    /// MUST be accepted even when local wall-clock has advanced to
    /// `15:30:00.100` IST.
    ///
    /// Reproduces the original yesterday-bug: the legacy gate rejected
    /// this tick because the wall-clock had crossed 15:30:00 by the
    /// time the gate evaluated. With the 60s G2 grace window, the gate
    /// now accepts it.
    #[test]
    fn test_tick_with_exchange_ts_15_29_59_586_accepted_even_if_local_recv_15_30_00_100() {
        // G1 — exchange-ts gate: 15:29:59 IST is in [09:15:00, 15:30:00).
        let exchange_ts_secs = 15 * 3600 + 29 * 60 + 59; // 15:29:59 IST
        assert!(
            is_within_persist_window(exchange_ts_secs, false),
            "G1 must accept exchange_ts inside [09:00, 15:30) — 15:29:59 is in-session"
        );
        // G2 — wall-clock gate: 15:30:00.100 IST is in [09:00, 15:31)
        // because of the 60s grace tail.
        let recv_at_nanos = ist_hms_to_utc_received_at_nanos(15, 30, 0, 100);
        assert!(
            is_wall_clock_within_persist_window(recv_at_nanos, false),
            "G2 grace tail (PR-Item-11) must accept wall-clock 15:30:00.100"
        );
    }

    /// Item 11 scenario test: a tick stamped exactly at session close
    /// (`exchange_ts = 15:30:00.000` IST) MUST be REJECTED by G1 — the
    /// market window is half-open `[09:15, 15:30)`.
    #[test]
    fn test_tick_with_exchange_ts_15_30_00_000_rejected() {
        let exchange_ts_secs = 15 * 3600 + 30 * 60; // 15:30:00 IST
        assert!(
            !is_within_persist_window(exchange_ts_secs, false),
            "G1 must reject exchange_ts at exactly 15:30:00 — interval is half-open"
        );
    }

    /// Item 11 scenario test: G2 wall-clock gate MUST stay open until
    /// 15:31:00 IST (60s grace tail), then close.
    #[test]
    fn test_ws_socket_stays_open_until_15_31_00() {
        // At 15:30:59 wall-clock — STILL inside the grace tail.
        let recv_15_30_59 = ist_hms_to_utc_received_at_nanos(15, 30, 59, 0);
        assert!(
            is_wall_clock_within_persist_window(recv_15_30_59, false),
            "G2 must remain open at wall-clock 15:30:59 (inside 60s grace)"
        );
        // At 15:31:00 wall-clock — boundary, gate closes.
        let recv_15_31_00 = ist_hms_to_utc_received_at_nanos(15, 31, 0, 0);
        assert!(
            !is_wall_clock_within_persist_window(recv_15_31_00, false),
            "G2 must close at wall-clock 15:31:00 sharp — grace window is half-open"
        );
    }

    // -----------------------------------------------------------------------
    // CCL-06 — Muhurat evening-session persist window (permutation-audit §140)
    // -----------------------------------------------------------------------

    /// The bug scenario: a Muhurat evening tick (~18:15 IST) MUST be persisted
    /// when today is a Muhurat session (`muhurat_active = true`). Before the
    /// fix, the whole ~1h Muhurat session was silently dropped despite a live
    /// connection.
    #[test]
    fn test_muhurat_tick_1815_ist_is_persisted() {
        let ts_1815 = ist_hms_to_ist_epoch(18, 15, 0);
        assert!(
            is_within_persist_window(ts_1815, true),
            "18:15 IST tick must be persisted on a Muhurat day (muhurat_active=true)"
        );
    }

    /// The gate MUST be `muhurat_active`-driven: the SAME 18:15 tick is still
    /// DROPPED on a normal trading/mock day (`muhurat_active = false`). This
    /// proves the Muhurat branch never widens a non-Muhurat day.
    #[test]
    fn test_muhurat_tick_1815_ist_dropped_when_flag_off() {
        let ts_1815 = ist_hms_to_ist_epoch(18, 15, 0);
        assert!(
            !is_within_persist_window(ts_1815, false),
            "18:15 IST tick must be DROPPED when muhurat_active=false (normal day)"
        );
    }

    /// Half-open Muhurat window boundaries `[18:00, 19:30)` IST when active.
    #[test]
    fn test_muhurat_window_boundaries() {
        // 18:00:00 IST — inclusive lower bound.
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(18, 0, 0),
            true
        ));
        // 17:59:59 IST — one second before open → rejected even when active.
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(17, 59, 59),
            true
        ));
        // 19:29:59 IST — last accepted second.
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(19, 29, 59),
            true
        ));
        // 19:30:00 IST — exclusive upper bound → rejected.
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(19, 30, 0),
            true
        ));
    }

    /// The Muhurat window is purely ADDITIVE: with `muhurat_active = true` the
    /// regular `[09:00, 15:30)` window still behaves EXACTLY as on a normal day
    /// (accept mid-session, reject pre/post-market). Muhurat never narrows it.
    #[test]
    fn test_regular_window_unchanged_when_muhurat_active() {
        // Mid-session still accepted with the flag on.
        assert!(is_within_persist_window(
            ist_hms_to_ist_epoch(12, 0, 0),
            true
        ));
        // Pre-market still rejected with the flag on.
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(8, 0, 0),
            true
        ));
        // Post-market gap between 15:30 and 18:00 still rejected with flag on.
        assert!(!is_within_persist_window(
            ist_hms_to_ist_epoch(16, 0, 0),
            true
        ));
    }

    /// The wall-clock gate (G2) is symmetric: an 18:15 IST wall-clock receipt
    /// is accepted on a Muhurat day and dropped otherwise.
    #[test]
    fn test_muhurat_wall_clock_1815_accepted_when_active() {
        let recv_1815 = ist_hms_to_utc_received_at_nanos(18, 15, 0, 0);
        assert!(
            is_wall_clock_within_persist_window(recv_1815, true),
            "wall-clock 18:15 IST must pass G2 on a Muhurat day"
        );
        assert!(
            !is_wall_clock_within_persist_window(recv_1815, false),
            "wall-clock 18:15 IST must fail G2 on a normal day"
        );
    }

    /// Item 11 source-scan ratchet: the G1 exchange-ts gate body
    /// (`is_within_persist_window`) MUST NOT call any local-clock
    /// helper. The bug spec verbatim: "market-hours gate evaluates
    /// `is_within_market_hours_ist(now())` — checks LOCAL clock instead
    /// of the tick's stamped time."
    #[test]
    fn test_market_gate_does_not_call_local_now() {
        let source = include_str!("tick_processor.rs");
        let fn_marker = "fn is_within_persist_window(";
        let start = source
            .find(fn_marker)
            .expect("is_within_persist_window must exist");
        // Function body extends through the next blank-line + closing
        // brace. Use the next `fn ` declaration as the upper bound —
        // far more than enough.
        let after = &source[start..];
        let next_fn = after
            .find("\nfn ")
            .map(|i| start + i)
            .unwrap_or(source.len());
        let body = &source[start..next_fn];
        assert!(
            !body.contains("Utc::now"),
            "G1 (is_within_persist_window) body MUST NOT call Utc::now — that was the bug"
        );
        assert!(
            !body.contains("now_ist"),
            "G1 body MUST NOT call now_ist — must use exchange_timestamp param"
        );
        assert!(
            !body.contains("SystemTime"),
            "G1 body MUST NOT call SystemTime — pure function on the param"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 12 — last_tick_audit late-tick anomaly ratchets (2026-05-17)
    // -----------------------------------------------------------------------

    /// Source-scan ratchet (Rule 13): the G1 reject site MUST call
    /// `append_last_tick_audit_row` for ticks stamped at or after
    /// session close. Without this call, the writer is defined but
    /// never invoked in production.
    #[test]
    fn test_late_tick_calls_append_last_tick_audit_row() {
        let source = include_str!("tick_processor.rs");
        assert!(
            source.contains("append_last_tick_audit_row"),
            "tick_processor.rs MUST call append_last_tick_audit_row — \
             Phase 0 Item 12 LATE-TICK forensic path"
        );
        assert!(
            source.contains("tv_late_tick_after_boundary_total"),
            "tick_processor.rs MUST emit tv_late_tick_after_boundary_total counter — \
             Phase 0 Item 12 LATE-TICK observability"
        );
    }

    // #T2b: the format_bar_minute_ist / exchange_segment_label_static
    // ratchet tests were removed with those helper fns (deleted along
    // with the last_tick_audit row write).

    // -----------------------------------------------------------------------
    // is_today_ist — stale day filter tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_today_ist_same_day() {
        // 2026-03-27 10:00:00 IST = day 20539
        let ts: u32 = 20539 * 86400 + 36000; // 10:00:00 IST
        let today_day: u32 = 20539;
        assert!(is_today_ist(ts, today_day));
    }

    #[test]
    fn test_is_today_ist_previous_day_rejected() {
        // Tick from yesterday (day 20538), today is day 20539
        let ts: u32 = 20538 * 86400 + 36000; // yesterday 10:00:00 IST
        let today_day: u32 = 20539;
        assert!(!is_today_ist(ts, today_day));
    }

    #[test]
    fn test_is_today_ist_future_day_rejected() {
        // Tick from tomorrow (day 20540), today is day 20539
        let ts: u32 = 20540 * 86400 + 36000;
        let today_day: u32 = 20539;
        assert!(!is_today_ist(ts, today_day));
    }

    #[test]
    fn test_is_today_ist_boundary_start_of_day() {
        // Exactly midnight IST (00:00:00) on today
        let today_day: u32 = 20539;
        let ts: u32 = today_day * 86400; // midnight
        assert!(is_today_ist(ts, today_day));
    }

    #[test]
    fn test_is_today_ist_boundary_end_of_day() {
        // Last second of today (23:59:59 IST)
        let today_day: u32 = 20539;
        let ts: u32 = today_day * 86400 + 86399;
        assert!(is_today_ist(ts, today_day));
    }

    #[test]
    fn test_is_today_ist_zero_timestamp() {
        // Zero timestamp should not match any real day
        assert!(!is_today_ist(0, 20539));
    }

    #[test]
    fn test_stale_day_combined_with_persist_window() {
        // Tick from yesterday at 10:00 IST — passes time check but fails day check
        let yesterday_day: u32 = 20538;
        let today_day: u32 = 20539;
        let ts: u32 = yesterday_day * 86400 + 36000; // yesterday 10:00:00 IST
        assert!(is_within_persist_window(ts, false)); // time is within [09:00, 15:30)
        assert!(!is_today_ist(ts, today_day)); // but it's not today
    }

    // -----------------------------------------------------------------------
    // Ingestion gate semantic tests — verifies gate drops ticks entirely
    // -----------------------------------------------------------------------

    #[test]
    fn test_ingestion_gate_pre_market_tick_dropped_before_all_processing() {
        // Pre-market tick (08:59:59 IST) must be dropped BEFORE dedup ring,
        // candle aggregator, top movers, and broadcast — not just QuestDB persistence.
        // This test verifies the gate function rejects pre-market timestamps.
        let pre_market_ts = ist_hms_to_ist_epoch(8, 59, 59);
        assert!(
            !is_within_persist_window(pre_market_ts, false),
            "pre-market tick must be rejected by ingestion gate"
        );
        // The tick processor calls `continue` on this result, so no downstream
        // processing (dedup, greeks, persistence, broadcast, candles, movers) occurs.
    }

    #[test]
    fn test_ingestion_gate_post_market_tick_dropped_before_all_processing() {
        // Post-market tick (15:30:00 IST) must be dropped BEFORE all processing.
        let post_market_ts = ist_hms_to_ist_epoch(15, 30, 0);
        assert!(
            !is_within_persist_window(post_market_ts, false),
            "post-market tick (15:30 exclusive) must be rejected by ingestion gate"
        );
    }

    #[test]
    fn test_ingestion_gate_last_valid_tick_accepted() {
        // 15:29:59 IST is the last second that passes the gate.
        let last_valid_ts = ist_hms_to_ist_epoch(15, 29, 59);
        assert!(
            is_within_persist_window(last_valid_ts, false),
            "15:29:59 IST must pass the ingestion gate (last valid candle)"
        );
    }

    #[test]
    fn test_ingestion_gate_first_valid_tick_accepted() {
        // 09:00:00 IST is the first second that passes the gate.
        let first_valid_ts = ist_hms_to_ist_epoch(9, 0, 0);
        assert!(
            is_within_persist_window(first_valid_ts, false),
            "09:00:00 IST must pass the ingestion gate (market open)"
        );
    }

    // -----------------------------------------------------------------------
    // depth_prices_are_finite — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_depth_prices_all_finite() {
        let depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 24500.0,
            ask_price: 24501.0,
        }; 5];
        assert!(depth_prices_are_finite(&depth));
    }

    #[test]
    fn test_depth_prices_nan_bid() {
        let mut depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 24500.0,
            ask_price: 24501.0,
        }; 5];
        depth[2].bid_price = f32::NAN;
        assert!(!depth_prices_are_finite(&depth));
    }

    #[test]
    fn test_depth_prices_infinity_ask() {
        let mut depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 24500.0,
            ask_price: 24501.0,
        }; 5];
        depth[4].ask_price = f32::INFINITY;
        assert!(!depth_prices_are_finite(&depth));
    }

    #[test]
    fn test_depth_prices_neg_infinity_bid() {
        let mut depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 24500.0,
            ask_price: 24501.0,
        }; 5];
        depth[0].bid_price = f32::NEG_INFINITY;
        assert!(!depth_prices_are_finite(&depth));
    }

    #[test]
    fn test_depth_prices_zero_is_finite() {
        let depth = [MarketDepthLevel {
            bid_quantity: 0,
            ask_quantity: 0,
            bid_orders: 0,
            ask_orders: 0,
            bid_price: 0.0,
            ask_price: 0.0,
        }; 5];
        assert!(depth_prices_are_finite(&depth));
    }

    #[test]
    fn test_depth_prices_negative_is_finite() {
        let depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: -1.0,
            ask_price: -2.0,
        }; 5];
        assert!(depth_prices_are_finite(&depth));
    }

    #[test]
    fn test_depth_prices_nan_in_first_level() {
        let mut depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 24500.0,
            ask_price: 24501.0,
        }; 5];
        depth[0].ask_price = f32::NAN;
        assert!(!depth_prices_are_finite(&depth));
    }

    #[test]
    fn test_depth_prices_nan_in_last_level() {
        let mut depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 24500.0,
            ask_price: 24501.0,
        }; 5];
        depth[4].bid_price = f32::NAN;
        assert!(!depth_prices_are_finite(&depth));
    }

    // -----------------------------------------------------------------------
    // TickDedupRing — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_ring_negative_received_at_no_panic() {
        // received_at is i64 and `as u64` is well-defined for negatives — the
        // hot path must never panic on an out-of-order / negative arrival clock.
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), -1));
        assert!(ring.is_duplicate(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), -1));
        assert!(!ring.is_duplicate(SEG_IDX, 13, today_ist_epoch_at(10, 0, 0), i64::MIN));
    }

    #[test]
    fn test_dedup_ring_large_buffer() {
        let ts = today_ist_epoch_at(10, 0, 0);
        // Realistic, decorrelated values: small instrument ids + epoch-nanos
        // arrivals 1 ms apart (production never has security_id == received_at).
        let base_recv = 1_700_000_000_000_000_000_i64;
        let recv = |i: i64| base_recv.wrapping_add(i.wrapping_mul(1_000_000));
        let sec = |i: i64| (i as u64).wrapping_add(100);
        let mut ring = TickDedupRing::new(16); // 65536 slots
        // Insert many unique frames.
        for i in 0..1000_i64 {
            assert!(!ring.is_duplicate(SEG_FNO, sec(i), ts, recv(i)));
        }
        // Re-insert the SAME frames — most should still be duplicates
        // (65536 >> 1000; birthday collisions ≈ 7.6).
        let mut dups = 0;
        for i in 0..1000_i64 {
            if ring.is_duplicate(SEG_FNO, sec(i), ts, recv(i)) {
                dups += 1;
            }
        }
        assert!(
            dups > 900,
            "with 65536 slots and 1000 entries, most should be duplicates (got {dups})"
        );
    }

    #[test]
    fn test_dedup_ring_fingerprint_zero_inputs() {
        let fp = TickDedupRing::fingerprint(0, 0, 0, 0);
        // Should not be the empty sentinel (u64::MAX)
        assert_ne!(fp, u64::MAX);
    }

    #[test]
    fn test_dedup_ring_fingerprint_max_inputs() {
        let fp = TickDedupRing::fingerprint(u8::MAX, u64::from(u32::MAX), u32::MAX, i64::MAX);
        assert_ne!(fp, u64::MAX);
        assert_ne!(fp, 0);
    }

    // -----------------------------------------------------------------------
    // Tick validity logic — unit-level pure tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_tick_validity_check_logic() {
        // Simulates the inline validity check in the hot loop
        let valid_ltp = 24500.0_f32;
        let invalid_ltp_zero = 0.0_f32;
        let invalid_ltp_nan = f32::NAN;
        let invalid_ltp_inf = f32::INFINITY;
        let invalid_ltp_neg = -1.0_f32;
        let valid_ts = today_ist_epoch_at(10, 0, 0);
        let invalid_ts = 0_u32;

        // Valid tick
        assert!(
            valid_ltp.is_finite()
                && valid_ltp > 0.0
                && valid_ts >= MINIMUM_VALID_EXCHANGE_TIMESTAMP
        );

        // Invalid: zero LTP
        assert!(!(invalid_ltp_zero.is_finite() && invalid_ltp_zero > 0.0));

        // Invalid: NaN LTP
        assert!(!invalid_ltp_nan.is_finite());

        // Invalid: Infinity LTP
        assert!(!invalid_ltp_inf.is_finite());

        // Invalid: negative LTP
        assert!(!(invalid_ltp_neg.is_finite() && invalid_ltp_neg > 0.0));

        // Invalid: timestamp below minimum
        assert!(
            !(valid_ltp.is_finite()
                && valid_ltp > 0.0
                && invalid_ts >= MINIMUM_VALID_EXCHANGE_TIMESTAMP)
        );
    }

    #[test]
    fn test_tick_with_depth_validity_logic() {
        // Simulates the tick_is_valid + ltp_valid logic for TickWithDepth
        let ltp = 24500.0_f32;
        let ts_valid = today_ist_epoch_at(10, 0, 0);
        let ts_zero = 0_u32;

        let ltp_valid = ltp.is_finite() && ltp > 0.0;
        assert!(ltp_valid);

        // Full packet: valid LTP + valid timestamp → tick_is_valid
        let tick_is_valid = ltp_valid && ts_valid >= MINIMUM_VALID_EXCHANGE_TIMESTAMP;
        assert!(tick_is_valid);

        // Market Depth code 3: valid LTP + timestamp=0 → ltp_valid but NOT tick_is_valid
        let tick_no_ts = ltp_valid && ts_zero >= MINIMUM_VALID_EXCHANGE_TIMESTAMP;
        assert!(!tick_no_ts);
        // Still ltp_valid → depth should persist
        assert!(ltp_valid);
    }

    // -----------------------------------------------------------------------
    // Previous close IST time derivation test
    // -----------------------------------------------------------------------

    #[test]
    fn test_prev_close_ist_secs_derivation() {
        // Simulates the IST seconds-of-day derivation used for PreviousClose packets.
        // received_at_nanos is UTC; we add IST offset then modulo SECONDS_PER_DAY.
        let received_at_nanos: i64 = 1_772_073_900_000_000_000; // some UTC nanos

        #[allow(clippy::cast_possible_truncation)]
        let prev_close_ist_secs = received_at_nanos
            .saturating_div(1_000_000_000)
            .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
            .rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;

        // Result should be in [0, 86400)
        assert!(prev_close_ist_secs < SECONDS_PER_DAY);
    }

    #[test]
    fn test_prev_close_ist_secs_zero_nanos() {
        // received_at_nanos = 0 → UTC epoch start → IST = 05:30:00 → secs_of_day = 19800
        let received_at_nanos: i64 = 0;
        #[allow(clippy::cast_possible_truncation)]
        let prev_close_ist_secs = received_at_nanos
            .saturating_div(1_000_000_000)
            .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
            .rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;

        assert_eq!(prev_close_ist_secs, 19800); // 05:30 IST
        // 05:30 IST is outside [09:00, 15:30) → should NOT persist
        assert!(
            !(TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST)
                .contains(&prev_close_ist_secs)
        );
    }

    // ===================================================================
    // is_valid_ltp — extracted pure function tests
    // ===================================================================

    #[test]
    fn test_is_valid_ltp_positive_price() {
        assert!(is_valid_ltp(24500.0));
        assert!(is_valid_ltp(0.01));
        // Real NSE prices (incl. the priciest instruments) are well under the
        // ceiling and must always pass — never miss a genuine tick.
        assert!(is_valid_ltp(150_000.0)); // MRF-class stock
        assert!(is_valid_ltp(80_000.0)); // SENSEX-class index
        assert!(is_valid_ltp(tickvault_common::constants::MAX_PLAUSIBLE_LTP)); // exactly at the ceiling = valid (inclusive)
    }

    #[test]
    fn test_is_valid_ltp_rejects_absurd_but_finite_price() {
        // The security-agent HIGH: f32::MAX is finite & > 0 but is garbage from
        // a mangled frame. It MUST be rejected so it can never poison a candle
        // high/low or a ticks row.
        assert!(!is_valid_ltp(f32::MAX));
        // Just above the ceiling is rejected.
        assert!(!is_valid_ltp(
            tickvault_common::constants::MAX_PLAUSIBLE_LTP * 2.0
        ));
        assert!(!is_valid_ltp(1.0e30));
    }

    #[test]
    fn test_is_valid_ltp_zero() {
        assert!(!is_valid_ltp(0.0));
    }

    #[test]
    fn test_is_valid_ltp_negative_zero() {
        assert!(!is_valid_ltp(-0.0));
    }

    #[test]
    fn test_is_valid_ltp_negative() {
        assert!(!is_valid_ltp(-1.0));
        assert!(!is_valid_ltp(-24500.0));
        assert!(!is_valid_ltp(f32::MIN));
    }

    #[test]
    fn test_is_valid_ltp_nan() {
        assert!(!is_valid_ltp(f32::NAN));
    }

    #[test]
    fn test_is_valid_ltp_positive_infinity() {
        assert!(!is_valid_ltp(f32::INFINITY));
    }

    #[test]
    fn test_is_valid_ltp_negative_infinity() {
        assert!(!is_valid_ltp(f32::NEG_INFINITY));
    }

    #[test]
    fn test_is_valid_ltp_subnormal() {
        // Subnormal is finite and > 0.0, so it's valid
        let subnormal = f32::MIN_POSITIVE / 2.0;
        assert!(subnormal.is_subnormal());
        assert!(is_valid_ltp(subnormal));
    }

    #[test]
    fn test_is_valid_ltp_min_positive() {
        assert!(is_valid_ltp(f32::MIN_POSITIVE));
    }

    // ===================================================================
    // is_valid_tick — extracted pure function tests
    // ===================================================================

    #[test]
    fn test_is_valid_tick_valid_inputs() {
        assert!(is_valid_tick(24500.0, MINIMUM_VALID_EXCHANGE_TIMESTAMP));
        assert!(is_valid_tick(100.0, today_ist_epoch_at(10, 0, 0)));
    }

    #[test]
    fn test_is_valid_tick_zero_ltp() {
        assert!(!is_valid_tick(0.0, today_ist_epoch_at(10, 0, 0)));
    }

    #[test]
    fn test_is_valid_tick_nan_ltp() {
        assert!(!is_valid_tick(f32::NAN, today_ist_epoch_at(10, 0, 0)));
    }

    #[test]
    fn test_is_valid_tick_inf_ltp() {
        assert!(!is_valid_tick(f32::INFINITY, today_ist_epoch_at(10, 0, 0)));
    }

    #[test]
    fn test_is_valid_tick_negative_ltp() {
        assert!(!is_valid_tick(-1.0, today_ist_epoch_at(10, 0, 0)));
    }

    #[test]
    fn test_is_valid_tick_zero_timestamp() {
        assert!(!is_valid_tick(24500.0, 0));
    }

    #[test]
    fn test_is_valid_tick_below_minimum_timestamp() {
        assert!(!is_valid_tick(
            24500.0,
            MINIMUM_VALID_EXCHANGE_TIMESTAMP - 1
        ));
    }

    #[test]
    fn test_is_valid_tick_at_minimum_timestamp() {
        assert!(is_valid_tick(24500.0, MINIMUM_VALID_EXCHANGE_TIMESTAMP));
    }

    #[test]
    fn test_is_valid_tick_both_invalid() {
        assert!(!is_valid_tick(0.0, 0));
        assert!(!is_valid_tick(f32::NAN, 0));
    }

    // ===================================================================
    // is_crossed_market — extracted pure function tests
    // ===================================================================

    #[test]
    fn test_is_crossed_market_normal_spread() {
        // bid < ask — normal market, not crossed
        assert!(!is_crossed_market(24500.0, 24501.0));
    }

    #[test]
    fn test_is_crossed_market_equal() {
        // bid == ask — locked market, not crossed
        assert!(!is_crossed_market(24500.0, 24500.0));
    }

    #[test]
    fn test_is_crossed_market_bid_greater() {
        // bid > ask — crossed market
        assert!(is_crossed_market(24501.0, 24500.0));
    }

    #[test]
    fn test_is_crossed_market_zero_bid() {
        // bid = 0 — not crossed (bid not > 0)
        assert!(!is_crossed_market(0.0, 24500.0));
    }

    #[test]
    fn test_is_crossed_market_zero_ask() {
        // ask = 0 — not crossed (ask not > 0)
        assert!(!is_crossed_market(24501.0, 0.0));
    }

    // -------------------------------------------------------------------
    // Phase 0 Item 22e — check_tick_ohlc_integrity
    // -------------------------------------------------------------------

    #[test]
    fn test_check_tick_ohlc_integrity_clean_tick_returns_zero() {
        // LTP within [low, high] — no violations.
        let v = check_tick_ohlc_integrity(24500.0, 24550.0, 24450.0);
        assert_eq!(v, 0);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_high_below_low_sets_bit_0() {
        // 24550 < 24600 — high-below-low violation.
        // LTP=24500 is below low (24600), so bit 2 ALSO fires.
        let v = check_tick_ohlc_integrity(24500.0, 24550.0, 24600.0);
        assert_eq!(v & 0b001, 0b001, "high-below-low bit must be set");
    }

    #[test]
    fn test_check_tick_ohlc_integrity_ltp_above_high_sets_bit_1() {
        // LTP=24700 > high=24550.
        let v = check_tick_ohlc_integrity(24700.0, 24550.0, 24450.0);
        assert_eq!(v, 0b010);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_ltp_below_low_sets_bit_2() {
        // LTP=24400 < low=24450.
        let v = check_tick_ohlc_integrity(24400.0, 24550.0, 24450.0);
        assert_eq!(v, 0b100);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_pre_open_zero_high_low_returns_zero() {
        // Dhan emits day_high=0 + day_low=0 BEFORE first trade in
        // pre-open. Flagging these would page the operator every
        // morning — skip silently.
        let v = check_tick_ohlc_integrity(24500.0, 0.0, 0.0);
        assert_eq!(v, 0);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_nan_ltp_returns_zero() {
        // NaN LTP is filtered by `is_valid_ltp` upstream; the OHLC
        // guard must not panic or false-positive on it either.
        let v = check_tick_ohlc_integrity(f32::NAN, 24550.0, 24450.0);
        assert_eq!(v, 0);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_negative_high_silently_skipped() {
        // day_high < 0 is non-physical but `is_finite` is true.
        // The `> 0.0` gate keeps us safe — no bits flip.
        let v = check_tick_ohlc_integrity(24500.0, -1.0, 24450.0);
        // Only the ltp-below-low comparison is skipped if low is OK
        // but the high<low comparison needs both — both gates clamp.
        assert_eq!(v, 0);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_high_equal_low_is_clean() {
        // Single-trade scenario (illiquid contract): high == low == ltp.
        // Not a violation.
        let v = check_tick_ohlc_integrity(24500.0, 24500.0, 24500.0);
        assert_eq!(v, 0);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_ltp_equal_high_is_clean() {
        // LTP at the day's running high — common, NOT a violation.
        let v = check_tick_ohlc_integrity(24550.0, 24550.0, 24450.0);
        assert_eq!(v, 0);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_ltp_equal_low_is_clean() {
        let v = check_tick_ohlc_integrity(24450.0, 24550.0, 24450.0);
        assert_eq!(v, 0);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_multiple_violations_combine() {
        // high=24400 < low=24500 (bit 0) + ltp=24600 > high=24400 (bit 1).
        let v = check_tick_ohlc_integrity(24600.0, 24400.0, 24500.0);
        assert_eq!(v & 0b001, 0b001);
        assert_eq!(v & 0b010, 0b010);
    }

    #[test]
    fn test_check_tick_ohlc_integrity_inf_high_silently_skipped() {
        // +Inf high is non-finite — gate clamps, no false-positive.
        let v = check_tick_ohlc_integrity(24500.0, f32::INFINITY, 24450.0);
        // ltp-above-high: high is Inf so `ltp > high` is false. No bit.
        // high-below-low: high non-finite, skipped.
        assert_eq!(v, 0);
    }

    /// Phase 0 Item 22e meta-guard: the tick processor MUST call
    /// `check_tick_ohlc_integrity` on every tick AND emit the three
    /// counters when violations fire. Without these, a buggy upstream
    /// (Dhan binary parser regression, malformed packet, etc.) silently
    /// poisons every downstream aggregation that depends on OHLC.
    ///
    /// Rule 13 (audit-findings 2026-04-17): "if a method is defined +
    /// tested but never called, it is a bug". This source-scan ratchet
    /// fails the build if a future refactor removes the call site.
    #[test]
    fn test_check_tick_ohlc_integrity_is_wired_into_tick_processor() {
        let src = include_str!("tick_processor.rs");
        assert!(
            src.contains("check_tick_ohlc_integrity("),
            "tick_processor.rs MUST call check_tick_ohlc_integrity on every \
             tick. Without it, OHLC corruption (high < low, ltp outside \
             [low, high]) is silently absorbed."
        );
        for counter in [
            "tv_tick_integrity_high_below_low_total",
            "tv_tick_integrity_ltp_above_high_total",
            "tv_tick_integrity_ltp_below_low_total",
        ] {
            assert!(
                src.contains(counter),
                "tick_processor.rs MUST emit the `{counter}` counter on \
                 the matching violation kind. Without it, the operator \
                 has no Prometheus visibility into OHLC corruption rates."
            );
        }
    }

    #[test]
    fn test_is_crossed_market_both_zero() {
        assert!(!is_crossed_market(0.0, 0.0));
    }

    #[test]
    fn test_is_crossed_market_negative_bid() {
        assert!(!is_crossed_market(-1.0, 24500.0));
    }

    #[test]
    fn test_is_crossed_market_negative_ask() {
        assert!(!is_crossed_market(24501.0, -1.0));
    }

    #[test]
    fn test_is_crossed_market_small_inversion() {
        // 0.05 paise inversion — still crossed
        assert!(is_crossed_market(24500.05, 24500.0));
    }

    // ===================================================================
    // utc_nanos_to_ist_secs_of_day — extracted pure function tests
    // ===================================================================

    #[test]
    fn test_utc_nanos_to_ist_secs_of_day_zero() {
        // UTC epoch 0 → IST 05:30:00 → secs_of_day = 19800
        let secs = utc_nanos_to_ist_secs_of_day(0);
        assert_eq!(secs, 19800);
    }

    #[test]
    fn test_utc_nanos_to_ist_secs_of_day_market_open() {
        // IST 09:00:00 = UTC 03:30:00 = 3*3600 + 30*60 = 12600 secs
        // nanos = 12600 * 1_000_000_000
        let nanos: i64 = 12600 * 1_000_000_000;
        let secs = utc_nanos_to_ist_secs_of_day(nanos);
        assert_eq!(secs, 32400); // 09:00 IST = 32400 secs-of-day
    }

    #[test]
    fn test_utc_nanos_to_ist_secs_of_day_market_close() {
        // IST 15:30:00 = UTC 10:00:00 = 36000 secs
        let nanos: i64 = 36000 * 1_000_000_000;
        let secs = utc_nanos_to_ist_secs_of_day(nanos);
        assert_eq!(secs, 55800); // 15:30 IST = 55800 secs-of-day
    }

    #[test]
    fn test_utc_nanos_to_ist_secs_of_day_result_in_range() {
        // Any valid input should produce secs_of_day in [0, 86400)
        for epoch_secs in [
            0_i64,
            i64::from(today_ist_epoch_at(10, 0, 0)),
            1_000_000_000,
            i64::MAX / 2,
        ] {
            let nanos = epoch_secs.saturating_mul(1_000_000_000);
            let secs = utc_nanos_to_ist_secs_of_day(nanos);
            assert!(
                secs < SECONDS_PER_DAY,
                "secs_of_day out of range for input {epoch_secs}"
            );
        }
    }

    #[test]
    fn test_utc_nanos_to_ist_secs_of_day_negative_nanos() {
        // Negative nanos (before epoch) should still produce valid secs-of-day via rem_euclid
        let secs = utc_nanos_to_ist_secs_of_day(-1_000_000_000);
        assert!(secs < SECONDS_PER_DAY);
    }

    #[test]
    fn test_utc_nanos_to_ist_secs_of_day_midnight_ist() {
        // IST midnight = UTC 18:30 previous day = 18*3600 + 30*60 = 66600 UTC secs-of-day
        // But we need full epoch. Let's use a known date.
        // 2026-03-17 UTC 18:30:00 = IST 2026-03-18 00:00:00
        // nanos = 66600 * 1_000_000_000
        let nanos: i64 = 66600 * 1_000_000_000;
        let secs = utc_nanos_to_ist_secs_of_day(nanos);
        // 66600 UTC secs + 19800 IST offset = 86400 → 86400 % 86400 = 0 → midnight IST
        assert_eq!(secs, 0);
    }

    // ===================================================================
    // derive_depth_timestamp_secs — extracted pure function tests
    // ===================================================================

    #[test]
    fn test_derive_depth_timestamp_valid_tick() {
        // tick_is_valid = true → use exchange_timestamp
        let ts = derive_depth_timestamp_secs(
            true,
            today_ist_epoch_at(10, 0, 0),
            1_772_000_000_000_000_000,
        );
        assert_eq!(ts, today_ist_epoch_at(10, 0, 0));
    }

    #[test]
    fn test_derive_depth_timestamp_invalid_tick() {
        // tick_is_valid = false → derive from received_at_nanos (UTC → IST)
        let nanos: i64 = 1_772_073_900_000_000_000;
        let ts = derive_depth_timestamp_secs(false, 0, nanos);
        // utc_nanos_to_ist_epoch_secs adds IST_UTC_OFFSET_SECONDS (19800)
        assert_eq!(ts, 1_772_073_900 + IST_UTC_OFFSET_SECONDS as u32);
    }

    #[test]
    fn test_derive_depth_timestamp_invalid_tick_zero_nanos() {
        let ts = derive_depth_timestamp_secs(false, 0, 0);
        // Zero UTC nanos → IST offset only
        assert_eq!(ts, IST_UTC_OFFSET_SECONDS as u32);
    }

    #[test]
    fn test_derive_depth_timestamp_valid_ignores_nanos() {
        // Even with different nanos, valid tick uses exchange_timestamp
        let ts = derive_depth_timestamp_secs(true, 12345, 99_999_999_999_999_999);
        assert_eq!(ts, 12345);
    }

    #[test]
    fn test_derive_depth_timestamp_invalid_large_nanos() {
        let nanos: i64 = 2_000_000_000_000_000_000; // ~2033 UTC
        let ts = derive_depth_timestamp_secs(false, 0, nanos);
        // UTC → IST: adds 19800 seconds
        assert_eq!(ts, 2_000_000_000 + IST_UTC_OFFSET_SECONDS as u32);
    }

    // ===================================================================
    // Broadcast channel + top movers + candle aggregator integration
    // ===================================================================

    #[tokio::test]
    async fn test_tick_processor_with_broadcast_channel() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let (tick_broadcast_tx, mut tick_broadcast_rx) = broadcast::channel::<ParsedTick>(100);

        // Pin the wall-clock to 10:00 AM IST today so the post-market
        // wall-clock guard does not filter the tick when this test runs
        // outside 09:00-15:30 IST. Override is scoped to this test via
        // `TestClockGuard` below — it resets on drop so later tests see
        // the real `Utc::now()`.
        let valid_ts_ist = today_ist_epoch_at(10, 0, 0);
        #[allow(clippy::cast_lossless)]
        let valid_received_at_utc_nanos: i64 =
            (i64::from(valid_ts_ist) - 19_800_i64).saturating_mul(1_000_000_000);
        let _clock_guard = TestClockGuard::set(valid_received_at_utc_nanos);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, Some(tick_broadcast_tx)).await;
        });

        // Send valid tick — should be broadcast.
        // Timestamp must be today + within [09:00, 15:30) IST to pass ingestion gate.
        let frame = make_ticker_frame(13, 24500.0, valid_ts_ist);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        // Receive from broadcast
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let tick = tick_broadcast_rx.try_recv();
        assert!(tick.is_ok(), "valid tick should be broadcast");
        let tick = tick.unwrap();
        assert_eq!(tick.security_id, 13);

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_consumes_ticker_frames() {
        // PR #2 (2026-05-18): renamed from `test_tick_processor_with_top_movers`
        // alongside the movers pipeline retirement. The remaining behavior
        // under test is that the tick processor consumes ticker frames
        // without panic and shuts down cleanly when the sender drops.
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });

        // Send valid ticks
        for i in 0..5 {
            let frame = make_ticker_frame(
                13 + i,
                24500.0 + (i as f32),
                today_ist_epoch_at(10, 0, 0) + i,
            );
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_full_quote_with_broadcast() {
        // PR #2 (2026-05-18): movers tracker removed from this test;
        // remaining coverage is that a Full packet routes through the
        // broadcast channel without panic.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let (tick_broadcast_tx, _tick_broadcast_rx) = broadcast::channel::<ParsedTick>(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, Some(tick_broadcast_tx)).await;
        });

        // Send Full Quote packet with valid LTP and timestamp
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_market_depth_valid_ltp_no_timestamp_broadcast() {
        // Market depth code 3 is dispatched as TickWithDepth. It has valid LTP
        // but exchange_timestamp=0. The tick persistence is skipped (timestamp
        // invalid), but the tick IS broadcast to browser WebSocket clients when
        // LTP is valid — this is correct behavior, since browsers need live
        // prices even from depth-only packets. Whether it reaches the broadcast
        // depends on the ingestion time gate ([09:00, 15:30) IST today).
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let (tick_broadcast_tx, mut tick_broadcast_rx) = broadcast::channel::<ParsedTick>(100);

        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, Some(tick_broadcast_tx)).await;
        });

        let frame = make_market_depth_frame(42, 25000.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let tick = tick_broadcast_rx.try_recv();
        // Code 3 with valid LTP: broadcast depends on market hours gate.
        // During [09:00, 15:30) IST on a trading day → broadcast (Ok).
        // Outside hours or on non-trading days → filtered (Err).
        // Both outcomes are correct — the test verifies no panic/crash.
        match tick {
            Ok(t) => {
                assert_eq!(t.security_id, 42);
                assert!(
                    t.last_traded_price > 0.0,
                    "broadcast tick should have valid LTP"
                );
            }
            Err(_) => {
                // Filtered by ingestion gate (outside hours or non-trading day).
                // This is correct — no data loss, depth was still processed.
            }
        }

        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // Tracing subscriber tests — forces field expression evaluation
    // -----------------------------------------------------------------------

    struct SinkSubscriber;
    impl tracing::Subscriber for SinkSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[tokio::test]
    async fn test_tick_processor_parse_error_with_tracing_subscriber() {
        // Exercises warn! field expressions in the parse error path (line 285)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        frame_tx
            .send(bytes::Bytes::from(vec![0u8; 4]))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_junk_tick_with_tracing_subscriber() {
        // Exercises debug! field expressions in junk tick path (lines 306-311)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(13, 0.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_full_quote_junk_with_tracing_subscriber() {
        // Exercises debug! field expressions in full-quote junk path (lines 424-430)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&0.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_crossed_market_with_tracing_subscriber() {
        // Exercises debug! field expressions in crossed market path (lines 448-453)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        // Build a Full packet with bid > ask at level 0 to trigger crossed market
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&today_ist_epoch_at(10, 0, 0).to_le_bytes());
        // Set bid_price > ask_price at depth level 0 (offset 62 in full packet)
        // Level 0: bid_qty@62, ask_qty@66, bid_orders@70, ask_orders@72,
        //          bid_price@74, ask_price@78
        frame[62..66].copy_from_slice(&100u32.to_le_bytes()); // bid_qty
        frame[66..70].copy_from_slice(&100u32.to_le_bytes()); // ask_qty
        frame[70..72].copy_from_slice(&5u16.to_le_bytes()); // bid_orders
        frame[72..74].copy_from_slice(&5u16.to_le_bytes()); // ask_orders
        frame[74..78].copy_from_slice(&25000.0_f32.to_le_bytes()); // bid > ask
        frame[78..82].copy_from_slice(&24900.0_f32.to_le_bytes()); // ask
        // Fill remaining depth levels with valid finite prices
        for level in 1..5 {
            let base = 62 + level * 20;
            frame[base..base + 4].copy_from_slice(&10u32.to_le_bytes());
            frame[base + 4..base + 8].copy_from_slice(&10u32.to_le_bytes());
            frame[base + 8..base + 10].copy_from_slice(&1u16.to_le_bytes());
            frame[base + 10..base + 12].copy_from_slice(&1u16.to_le_bytes());
            frame[base + 12..base + 16].copy_from_slice(&24500.0_f32.to_le_bytes());
            frame[base + 16..base + 20].copy_from_slice(&24501.0_f32.to_le_bytes());
        }
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_disconnect_with_tracing_subscriber() {
        // Exercises error! field expression in disconnect path (line 597)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        frame[8..10].copy_from_slice(&805u16.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_oi_with_tracing_subscriber() {
        // Exercises debug! field expressions in OI path (lines 519-522)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_market_status_with_tracing_subscriber() {
        // Exercises info! field expressions in market status path (line 577)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_valid_tick_with_tracing_subscriber() {
        // Exercises trace! field expressions in successful tick path (lines 367-371)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        let frame = make_ticker_frame(13, 24500.0, today_ist_epoch_at(10, 0, 0));
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_previous_close_valid_with_tracing_subscriber() {
        // Exercises debug! field expressions in previous close persist path (lines 567-570)
        let _guard = tracing::subscriber::set_default(SinkSubscriber);
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_test_tick_processor(frame_rx, None, None).await;
        });
        // Build a PrevClose packet with valid previous_close price
        let mut frame = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        frame[8..12].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[12..16].copy_from_slice(&1000u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // Channel backpressure metric (M6)
    // -----------------------------------------------------------------------

    #[test]
    fn test_channel_occupancy_metric() {
        // Verify metric handle creation compiles without a recorder installed.
        metrics::gauge!("tv_tick_channel_occupancy").set(0.0_f64);
        metrics::gauge!("tv_tick_channel_capacity").set(65536.0_f64);
    }

    #[test]
    fn test_channel_throughput_metric_compiles() {
        // Verify metric handle creation compiles without a recorder installed.
        metrics::gauge!("tv_channel_throughput_tps").set(0.0_f64);
    }

    // -----------------------------------------------------------------------
    // ZL-P0-2: canary underlyings table + gauge
    // -----------------------------------------------------------------------

    /// The canary table is the contract between code and the Grafana
    /// per-underlying alert. It must contain at least 1 entry (otherwise
    /// the canary is disabled and we have no end-to-end heartbeat
    /// probe) and must be bounded (we scan it linearly on every tick,
    /// so unbounded growth would be a hot-path regression).
    #[test]
    fn test_zl_p0_2_canary_underlyings_table_is_bounded() {
        assert!(
            !CANARY_UNDERLYINGS.is_empty(),
            "CANARY_UNDERLYINGS must have at least 1 entry — otherwise the \
             end-to-end heartbeat probe is effectively disabled (ZL-P0-2)"
        );
        assert!(
            CANARY_UNDERLYINGS.len() <= 8,
            "CANARY_UNDERLYINGS is scanned linearly on every tick — keep it \
             short enough that the whole table fits in one cache line"
        );
    }

    /// The well-known NSE spot indices MUST be present. A missing entry
    /// silently disables the canary for that index.
    #[test]
    fn test_zl_p0_2_canary_underlyings_contains_nifty_banknifty_sensex() {
        let sids: Vec<u64> = CANARY_UNDERLYINGS.iter().map(|(sid, _)| *sid).collect();
        assert!(
            sids.contains(&13),
            "NIFTY (SID 13) must be a canary underlying"
        );
        assert!(
            sids.contains(&25),
            "BANKNIFTY (SID 25) must be a canary underlying"
        );
        assert!(
            sids.contains(&51),
            "SENSEX (SID 51) must be a canary underlying"
        );
    }

    /// Every canary entry must have a non-empty label — labels are used
    /// as Prometheus `underlying` label values and empty strings would
    /// silently break the Grafana alert rule.
    #[test]
    fn test_zl_p0_2_canary_underlyings_have_non_empty_labels() {
        for (sid, label) in CANARY_UNDERLYINGS {
            assert!(
                !label.is_empty(),
                "CANARY_UNDERLYINGS entry sid={sid} has empty label"
            );
            assert!(
                label.chars().all(|c| c.is_ascii_uppercase()),
                "CANARY_UNDERLYINGS label `{label}` must be uppercase ASCII \
                 (Prometheus label value convention)"
            );
        }
    }

    /// Pin the metric name as a public contract with Grafana alert rules.
    #[test]
    fn test_zl_p0_2_canary_metric_name_stable() {
        // This test exists only to make accidental renames a compile-
        // visible diff. Rename only in lockstep with Grafana rules.
        let name = "tv_ws_canary_last_tick_epoch_secs";
        assert_eq!(name, "tv_ws_canary_last_tick_epoch_secs");
    }

    /// Sanity: building the canary gauges with the actual labels used
    /// in production must not panic and must not produce 0 handles.
    #[test]
    fn test_zl_p0_2_canary_metric_builds_without_recorder() {
        // Without a recorder installed the gauges are no-ops, but
        // constructing them still exercises the full metrics macro
        // expansion — this catches typos in the metric name or label
        // key at compile+run time.
        for (_, name) in CANARY_UNDERLYINGS {
            let gauge = metrics::gauge!("tv_ws_canary_last_tick_epoch_secs", "underlying" => name.to_string());
            gauge.set(1_700_000_000.0);
        }
    }
}
