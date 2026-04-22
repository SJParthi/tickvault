//! Per-minute price buffer for pre-open (09:08-09:12 IST) price capture.
//!
//! # Why this exists (Parthiban, 2026-04-20)
//!
//! At 09:12:30 IST the Phase 2 scheduler wakes up to subscribe stock F&O
//! contracts (ATM ± N CE/PE). To pick the right ATM strike for each stock,
//! we need the *finalised pre-open price* — Dhan's pre-open matching ends
//! at 09:08 and the same equilibrium LTP is streamed on each stock's
//! NSE_EQ subscription during the 09:08-09:15 buffer window.
//!
//! Per Parthiban's spec: use the **09:12 close** for each stock. If no tick
//! landed in the 09:12:00-09:12:59 bucket (stock had no pre-open activity
//! that minute), **backtrack** to 09:11 → 09:10 → 09:09 → 09:08. If all
//! five buckets are empty, skip that stock with a WARN + metric.
//!
//! # How
//!
//! 1. `PreOpenCloses` holds 5 `Option<f64>` slots indexed 0..=4 for
//!    minutes 09:08/09/10/11/12.
//! 2. `SharedPreOpenBuffer` is an `Arc<RwLock<HashMap<String, PreOpenCloses>>>`
//!    keyed by underlying stock symbol.
//! 3. `run_preopen_snapshot_task` subscribes to the tick broadcast and, for
//!    every NSE_EQ tick belonging to an F&O stock whose IST wall-clock
//!    minute is in 09:08..=09:12, overwrites the matching slot with the
//!    latest LTP (last-write-wins within a minute = the minute's close).
//! 4. Outside 09:08-09:12 IST the task is idle — matches the
//!    market-hours-aware background-worker rule from
//!    `.claude/rules/project/audit-findings-2026-04-17.md` Rule 3.
//!
//! # Hot-path guarantees
//!
//! - Bucketing is an O(1) index into a 5-slot array — no allocation.
//! - Lock is a `tokio::sync::RwLock`; writes held for microseconds only.
//! - The phase2 scheduler reads a snapshot ONCE per day at 09:12:30.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{FixedOffset, TimeZone, Timelike, Utc};
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::instrument_types::{FnoUniverse, UnderlyingKind};
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tokio::sync::RwLock;

/// Number of one-minute slots captured in the pre-open window (09:08..=09:12).
pub const PREOPEN_MINUTE_SLOTS: usize = 5;

/// Pre-open index underlyings whose 09:12 close is used to pick depth-20 +
/// depth-200 ATM strikes at 09:12:30 IST (mirror of the stock F&O dispatch
/// path — see `phase2_scheduler.rs` and `.claude/plans/active-plan.md`
/// items A + C). The security IDs come from Dhan's instrument master and are
/// fixed per `.claude/rules/project/depth-subscription.md`.
pub const PREOPEN_INDEX_UNDERLYINGS: &[(&str, u32)] = &[("NIFTY", 13), ("BANKNIFTY", 25)];

/// First captured minute, as seconds since IST midnight (09:08:00 IST).
pub const PREOPEN_FIRST_MINUTE_SECS_IST: u32 = 9 * 3600 + 8 * 60;

/// Last captured minute (exclusive upper bound), as seconds since IST midnight
/// (09:13:00 IST — slot 4 = 09:12:00..09:12:59).
pub const PREOPEN_LAST_MINUTE_SECS_IST: u32 = 9 * 3600 + 13 * 60;

/// Per-stock pre-open minute-close buffer.
///
/// Slots are indexed 0..5 = minutes 09:08/09/10/11/12. `None` = no tick
/// landed in that minute for this stock.
#[derive(Debug, Clone, Copy, Default)]
pub struct PreOpenCloses {
    /// Last LTP seen inside each minute bucket (or `None` if no tick arrived).
    pub closes: [Option<f64>; PREOPEN_MINUTE_SLOTS],
}

impl PreOpenCloses {
    /// Returns the first non-`None` price when scanning from the latest minute
    /// (09:12) backwards to 09:08.
    ///
    /// Matches Parthiban's spec: "9:12 close alone only right only then we
    /// will clearly know ... when 9.12 am close is not found then backtrack
    /// till 9.11 or 9.10 or 9.09 or 9.08".
    ///
    /// Returns `None` only when ALL 5 slots are empty.
    #[inline]
    pub fn backtrack_latest(&self) -> Option<f64> {
        for slot in self.closes.iter().rev() {
            if let Some(price) = slot
                && price.is_finite()
                && *price > 0.0
            {
                return Some(*price);
            }
        }
        None
    }

    /// Records the LTP for the minute indicated by `minute_index`
    /// (0 = 09:08, ..., 4 = 09:12). Last-write-wins inside the minute.
    #[inline]
    pub fn record(&mut self, minute_index: usize, price: f64) {
        if minute_index < PREOPEN_MINUTE_SLOTS && price.is_finite() && price > 0.0 {
            self.closes[minute_index] = Some(price);
        }
    }
}

/// Thread-safe shared buffer, one entry per F&O stock symbol.
pub type SharedPreOpenBuffer = Arc<RwLock<HashMap<String, PreOpenCloses>>>;

/// Creates an empty shared buffer. Capacity pre-sized for ~300 F&O stocks.
#[must_use]
pub fn new_shared_preopen_buffer() -> SharedPreOpenBuffer {
    // O(1) EXEMPT: boot-time allocation, 300 entries, never resized on hot path.
    Arc::new(RwLock::new(HashMap::with_capacity(320)))
}

/// Maps an IST wall-clock seconds-of-day to a pre-open minute slot index.
/// Returns `None` if the time is outside the 09:08..09:13 window.
#[inline]
#[must_use]
pub fn minute_index_for_ist_seconds(sec_of_day_ist: u32) -> Option<usize> {
    if !(PREOPEN_FIRST_MINUTE_SECS_IST..PREOPEN_LAST_MINUTE_SECS_IST).contains(&sec_of_day_ist) {
        return None;
    }
    // 60 seconds per minute; 0-indexed from 09:08.
    let minute_offset = (sec_of_day_ist - PREOPEN_FIRST_MINUTE_SECS_IST) / 60;
    Some(minute_offset as usize)
}

/// Maps a UTC epoch second to a pre-open minute slot index in IST terms.
///
/// Returns `None` outside 09:08..09:13 IST.
#[inline]
#[must_use]
pub fn minute_index_for_utc_epoch(utc_epoch_secs: i64) -> Option<usize> {
    let ist_offset = i64::from(IST_UTC_OFFSET_SECONDS);
    let ist_epoch = utc_epoch_secs.saturating_add(ist_offset);
    let sec_of_day = ist_epoch.rem_euclid(86_400) as u32;
    minute_index_for_ist_seconds(sec_of_day)
}

/// Records a price snapshot for a stock at a given UTC epoch timestamp.
///
/// No-op when the timestamp is outside the 09:08-09:12 IST window.
/// Intended to be called from the tick broadcast subscriber in production
/// and directly from tests to populate buckets.
pub async fn record_preopen_tick(
    buffer: &SharedPreOpenBuffer,
    symbol: &str,
    utc_epoch_secs: i64,
    price: f64,
) {
    let Some(minute_index) = minute_index_for_utc_epoch(utc_epoch_secs) else {
        return;
    };
    if !(price.is_finite() && price > 0.0) {
        return;
    }
    let mut guard = buffer.write().await;
    guard
        .entry(symbol.to_string())
        .or_default()
        .record(minute_index, price);
}

/// Reads a consistent snapshot of every stock's pre-open closes.
/// Used by the Phase 2 scheduler at 09:12:30 IST to pick ATM strikes.
pub async fn snapshot(buffer: &SharedPreOpenBuffer) -> HashMap<String, PreOpenCloses> {
    let guard = buffer.read().await;
    guard.clone()
}

/// Returns `true` if the current wall-clock time is inside the pre-open
/// snapshot window (09:08..09:13 IST). The snapshotter task uses this to
/// gate work outside the window per audit-findings Rule 3.
#[must_use]
pub fn is_within_preopen_window() -> bool {
    // `IST_UTC_OFFSET_SECONDS` (19800) is well within `FixedOffset::east_opt`'s
    // accepted range (±86_400), so `None` is structurally unreachable — but we
    // still treat it safely (return false) to satisfy the no-unwrap/no-expect
    // production lint.
    let Some(offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
        return false;
    };
    let now_ist = offset.from_utc_datetime(&Utc::now().naive_utc());
    let sec_of_day = (now_ist.time().hour() * 3600
        + now_ist.time().minute() * 60
        + now_ist.time().second()) as u32;
    minute_index_for_ist_seconds(sec_of_day).is_some()
}

// ---------------------------------------------------------------------------
// Snapshotter — tick-broadcast filter for F&O stock pre-open prices
// ---------------------------------------------------------------------------

/// Reason a tick was rejected by the snapshotter — kept as a typed
/// enum so we can label the Prometheus filtered counter without
/// allocating a `String` on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotterFilterReason {
    /// Tick was on a segment other than NSE_EQ — depth, derivatives,
    /// indices, etc. all live on different segments.
    WrongSegment,
    /// Tick was on NSE_EQ but the security_id doesn't belong to any
    /// F&O stock underlying in the universe (e.g. cash-only equity).
    NotFnoStock,
    /// Tick was on the right segment + an F&O stock, but its IST
    /// wall-clock minute was outside the 09:08..09:12 capture window.
    WrongMinute,
}

impl SnapshotterFilterReason {
    /// Static label for Prometheus — must NOT allocate.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::WrongSegment => "wrong_segment",
            Self::NotFnoStock => "not_fno_stock",
            Self::WrongMinute => "wrong_minute",
        }
    }
}

/// Outcome of feeding one tick into the snapshotter.
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotterOutcome {
    /// Tick was buffered into the matching minute slot.
    Buffered { symbol: String, minute_index: usize },
    /// Tick was rejected — caller increments the corresponding counter.
    Filtered(SnapshotterFilterReason),
}

/// Builds a `(security_id, exchange_segment)` → underlying-symbol lookup
/// table for every F&O **stock** in the universe. Indices are excluded
/// because Phase 2 only subscribes stock F&O — index option chains are
/// already handled in Phase 1.
///
/// Per `.claude/rules/project/security-id-uniqueness.md` (Rule I-P1-11),
/// the key MUST be `(security_id, segment)`, not `security_id` alone.
///
/// O(1) EXEMPT: boot-time lookup table, ~200 entries.
#[must_use]
pub fn build_fno_stock_lookup(universe: &FnoUniverse) -> HashMap<(u32, ExchangeSegment), String> {
    let mut out: HashMap<(u32, ExchangeSegment), String> =
        HashMap::with_capacity(universe.underlyings.len());
    for (symbol, ul) in &universe.underlyings {
        if ul.kind != UnderlyingKind::Stock {
            continue;
        }
        out.insert(
            (ul.price_feed_security_id, ul.price_feed_segment),
            symbol.clone(),
        );
    }
    out
}

/// Builds the unified pre-open lookup for the index underlyings in
/// `PREOPEN_INDEX_UNDERLYINGS`. Keyed by `(security_id, IdxI)` so it
/// merges cleanly with the F&O stock lookup (different segment half of the
/// composite key per I-P1-11).
///
/// O(1) EXEMPT: boot-time lookup table with ≤ `PREOPEN_INDEX_UNDERLYINGS.len()`
/// entries (today: 2 — NIFTY + BANKNIFTY).
#[must_use]
pub fn build_preopen_index_lookup() -> HashMap<(u32, ExchangeSegment), String> {
    let mut out: HashMap<(u32, ExchangeSegment), String> =
        HashMap::with_capacity(PREOPEN_INDEX_UNDERLYINGS.len());
    for (symbol, security_id) in PREOPEN_INDEX_UNDERLYINGS {
        out.insert((*security_id, ExchangeSegment::IdxI), (*symbol).to_string());
    }
    out
}

/// Builds the combined pre-open lookup used by `classify_tick` — F&O stocks
/// (NSE_EQ) + whitelisted indices (IDX_I). Composite key `(security_id,
/// segment)` guarantees no collision per I-P1-11.
///
/// O(1) EXEMPT: boot-time, ~220 entries total.
#[must_use]
pub fn build_preopen_combined_lookup(
    universe: &FnoUniverse,
) -> HashMap<(u32, ExchangeSegment), String> {
    let mut combined = build_fno_stock_lookup(universe);
    for ((security_id, seg), symbol) in build_preopen_index_lookup() {
        combined.insert((security_id, seg), symbol);
    }
    combined
}

/// Pure synchronous classifier — given a parsed tick + the combined lookup
/// table built by `build_preopen_combined_lookup`, decide whether the tick
/// should be buffered (and into which minute slot) or filtered.
///
/// Accepts both `NseEquity` (F&O stocks) and `IdxI` (whitelisted indices —
/// NIFTY + BANKNIFTY, used for depth ATM selection at 09:12:30).
///
/// Pure function = no I/O, no async, no allocation on the buffer
/// path. Caller is responsible for actually writing to the
/// `SharedPreOpenBuffer` and incrementing the metrics.
#[must_use]
pub fn classify_tick(
    tick: &ParsedTick,
    fno_stock_lookup: &HashMap<(u32, ExchangeSegment), String>,
) -> SnapshotterOutcome {
    // Step 1: segment filter — accept both F&O-stock feed segment (NSE_EQ)
    // AND IDX_I (major indices NIFTY + BANKNIFTY for depth ATM at 09:12:30).
    // All other segments (BSE, MCX, Currency, derivatives) are filtered.
    let Some(seg) = ExchangeSegment::from_byte(tick.exchange_segment_code) else {
        return SnapshotterOutcome::Filtered(SnapshotterFilterReason::WrongSegment);
    };
    if seg != ExchangeSegment::NseEquity && seg != ExchangeSegment::IdxI {
        return SnapshotterOutcome::Filtered(SnapshotterFilterReason::WrongSegment);
    }

    // Step 2: composite-key membership check. Key is `(security_id, segment)`
    // per I-P1-11 so the same numeric id across segments never collides.
    let Some(symbol) = fno_stock_lookup.get(&(tick.security_id, seg)) else {
        return SnapshotterOutcome::Filtered(SnapshotterFilterReason::NotFnoStock);
    };

    // Step 3: minute-window check. ParsedTick.exchange_timestamp is
    // an IST epoch second (Dhan convention — see `live-market-feed.md`
    // rule 14, LTT fields are IST). Treat 0 as "no timestamp".
    if tick.exchange_timestamp == 0 {
        return SnapshotterOutcome::Filtered(SnapshotterFilterReason::WrongMinute);
    }
    // Convert IST epoch → UTC epoch for our existing helper, which
    // adds the IST offset back internally.
    let utc_epoch =
        i64::from(tick.exchange_timestamp).saturating_sub(i64::from(IST_UTC_OFFSET_SECONDS));
    let Some(minute_index) = minute_index_for_utc_epoch(utc_epoch) else {
        return SnapshotterOutcome::Filtered(SnapshotterFilterReason::WrongMinute);
    };
    SnapshotterOutcome::Buffered {
        symbol: symbol.clone(),
        minute_index,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    // 09:08:00 IST = 03:38:00 UTC = UTC epoch offset from midnight = 3*3600+38*60
    fn ist_utc_epoch(hour: u32, minute: u32, second: u32) -> i64 {
        // Given an IST hh:mm:ss, return the corresponding UTC epoch second
        // on an arbitrary date (2026-04-20). Subtract 19800 to go UTC.
        let ist_naive = chrono::NaiveDate::from_ymd_opt(2026, 4, 20)
            .unwrap()
            .and_hms_opt(hour, minute, second)
            .unwrap();
        let ist_utc = ist_naive.and_utc().timestamp() - i64::from(IST_UTC_OFFSET_SECONDS);
        ist_utc
    }

    #[test]
    fn test_minute_index_for_ist_seconds_each_bucket() {
        assert_eq!(minute_index_for_ist_seconds(9 * 3600 + 8 * 60), Some(0));
        assert_eq!(
            minute_index_for_ist_seconds(9 * 3600 + 8 * 60 + 59),
            Some(0)
        );
        assert_eq!(minute_index_for_ist_seconds(9 * 3600 + 9 * 60), Some(1));
        assert_eq!(minute_index_for_ist_seconds(9 * 3600 + 10 * 60), Some(2));
        assert_eq!(minute_index_for_ist_seconds(9 * 3600 + 11 * 60), Some(3));
        assert_eq!(minute_index_for_ist_seconds(9 * 3600 + 12 * 60), Some(4));
    }

    #[test]
    fn test_minute_index_for_ist_seconds_outside_window() {
        // 09:07:59 — one second before window
        assert_eq!(
            minute_index_for_ist_seconds(9 * 3600 + 7 * 60 + 59),
            None,
            "09:07:59 must not map to a bucket"
        );
        // 09:13:00 — exclusive upper bound
        assert_eq!(
            minute_index_for_ist_seconds(9 * 3600 + 13 * 60),
            None,
            "09:13:00 must not map to a bucket (exclusive upper bound)"
        );
        // Market open 09:15 — well past window
        assert_eq!(minute_index_for_ist_seconds(9 * 3600 + 15 * 60), None);
        // Midnight
        assert_eq!(minute_index_for_ist_seconds(0), None);
    }

    #[test]
    fn test_backtrack_latest_prefers_09_12_over_earlier() {
        // All 5 buckets populated — must return the 09:12 value (slot 4).
        let closes = PreOpenCloses {
            closes: [
                Some(100.0),
                Some(101.0),
                Some(102.0),
                Some(103.0),
                Some(104.0),
            ],
        };
        assert_eq!(closes.backtrack_latest(), Some(104.0));
    }

    #[test]
    fn test_backtrack_latest_falls_back_to_09_11_when_09_12_missing() {
        let closes = PreOpenCloses {
            closes: [Some(100.0), Some(101.0), Some(102.0), Some(103.0), None],
        };
        assert_eq!(closes.backtrack_latest(), Some(103.0));
    }

    #[test]
    fn test_backtrack_latest_walks_all_way_to_09_08_when_only_that_minute_has_data() {
        let closes = PreOpenCloses {
            closes: [Some(95.5), None, None, None, None],
        };
        assert_eq!(
            closes.backtrack_latest(),
            Some(95.5),
            "only 09:08 slot populated — must return that value"
        );
    }

    #[test]
    fn test_backtrack_latest_returns_none_when_no_minute_has_data() {
        let closes = PreOpenCloses {
            closes: [None; PREOPEN_MINUTE_SLOTS],
        };
        assert!(closes.backtrack_latest().is_none());
    }

    #[test]
    fn test_backtrack_latest_ignores_non_finite_and_zero_prices() {
        let closes = PreOpenCloses {
            closes: [
                Some(100.0),
                Some(0.0),
                Some(f64::NAN),
                Some(f64::INFINITY),
                Some(-1.0),
            ],
        };
        // All later slots are rejected by the is_finite && >0 guard, so we
        // fall back to slot 0 (100.0).
        assert_eq!(closes.backtrack_latest(), Some(100.0));
    }

    #[test]
    fn test_record_last_write_wins_within_minute() {
        let mut closes = PreOpenCloses::default();
        closes.record(4, 101.0); // first 09:12 tick
        closes.record(4, 102.0); // later 09:12 tick
        closes.record(4, 103.5); // last 09:12 tick — this is the "close"
        assert_eq!(closes.closes[4], Some(103.5));
    }

    #[test]
    fn test_record_rejects_out_of_range_index() {
        let mut closes = PreOpenCloses::default();
        closes.record(PREOPEN_MINUTE_SLOTS, 100.0); // 5 is out of 0..5
        closes.record(99, 100.0);
        assert!(closes.closes.iter().all(|s| s.is_none()));
    }

    #[test]
    fn test_record_rejects_nonpositive_and_nonfinite() {
        let mut closes = PreOpenCloses::default();
        closes.record(0, 0.0);
        closes.record(1, -5.0);
        closes.record(2, f64::NAN);
        closes.record(3, f64::INFINITY);
        closes.record(4, f64::NEG_INFINITY);
        assert!(closes.closes.iter().all(|s| s.is_none()));
    }

    #[tokio::test]
    async fn test_record_preopen_tick_buckets_into_correct_minute() {
        let buffer = new_shared_preopen_buffer();
        // 09:12:15 IST tick for RELIANCE
        let ts = ist_utc_epoch(9, 12, 15);
        record_preopen_tick(&buffer, "RELIANCE", ts, 2847.5).await;
        let snap = snapshot(&buffer).await;
        let reliance = snap.get("RELIANCE").expect("RELIANCE bucket must exist");
        assert_eq!(reliance.closes[4], Some(2847.5));
        assert!(reliance.closes[0..4].iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn test_record_preopen_tick_ignores_outside_window() {
        let buffer = new_shared_preopen_buffer();
        // 09:15:00 — main session open, outside the 09:08..09:13 window.
        let ts = ist_utc_epoch(9, 15, 0);
        record_preopen_tick(&buffer, "RELIANCE", ts, 2847.5).await;
        let snap = snapshot(&buffer).await;
        assert!(
            snap.get("RELIANCE").is_none(),
            "no bucket should have been created for a 09:15 tick"
        );
    }

    #[tokio::test]
    async fn test_snapshot_returns_independent_clone() {
        let buffer = new_shared_preopen_buffer();
        let ts = ist_utc_epoch(9, 10, 0);
        record_preopen_tick(&buffer, "TCS", ts, 3500.0).await;
        let snap1 = snapshot(&buffer).await;
        // Mutate the original buffer — snap1 must not see the change.
        let ts2 = ist_utc_epoch(9, 12, 0);
        record_preopen_tick(&buffer, "TCS", ts2, 3510.0).await;
        assert_eq!(snap1.get("TCS").unwrap().closes[4], None);
        let snap2 = snapshot(&buffer).await;
        assert_eq!(snap2.get("TCS").unwrap().closes[4], Some(3510.0));
    }

    #[test]
    fn test_preopen_index_underlyings_contains_nifty_and_banknifty() {
        // Per .claude/rules/project/depth-subscription.md: NIFTY=13, BANKNIFTY=25.
        // These feed the depth-20 + depth-200 ATM selection at 09:12:30.
        let map: HashMap<&str, u32> = PREOPEN_INDEX_UNDERLYINGS.iter().copied().collect();
        assert_eq!(map.get("NIFTY"), Some(&13));
        assert_eq!(map.get("BANKNIFTY"), Some(&25));
    }

    #[test]
    fn test_build_preopen_index_lookup_keyed_by_composite() {
        // I-P1-11: key MUST be (security_id, segment). IdxI segment for
        // indices so we never collide with any NSE_EQ id reuse.
        let lookup = build_preopen_index_lookup();
        assert_eq!(
            lookup.get(&(13, ExchangeSegment::IdxI)),
            Some(&"NIFTY".to_string())
        );
        assert_eq!(
            lookup.get(&(25, ExchangeSegment::IdxI)),
            Some(&"BANKNIFTY".to_string())
        );
        // Wrong segment must not match even with same id.
        assert!(lookup.get(&(13, ExchangeSegment::NseEquity)).is_none());
        assert!(lookup.get(&(25, ExchangeSegment::NseEquity)).is_none());
    }

    #[test]
    fn test_build_preopen_index_lookup_contains_exactly_two_entries() {
        let lookup = build_preopen_index_lookup();
        assert_eq!(lookup.len(), PREOPEN_INDEX_UNDERLYINGS.len());
        assert_eq!(lookup.len(), 2, "today we subscribe NIFTY + BANKNIFTY only");
    }

    #[test]
    fn test_build_preopen_combined_lookup_includes_indices() {
        // Use an empty universe — the combined lookup must still contain
        // the whitelisted index entries sourced from
        // `PREOPEN_INDEX_UNDERLYINGS`. Stock-side merge is covered by
        // `build_fno_stock_lookup`'s own tests and integration tests.
        use std::time::Duration;
        use tickvault_common::instrument_types::UniverseBuildMetadata;
        let universe = FnoUniverse {
            underlyings: HashMap::new(),
            derivative_contracts: HashMap::new(),
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: String::new(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: Duration::from_millis(0),
                build_timestamp: chrono::FixedOffset::east_opt(0)
                    .expect("zero offset")
                    .from_utc_datetime(&Utc::now().naive_utc()),
            },
        };
        let combined = build_preopen_combined_lookup(&universe);

        // With an empty universe the combined lookup is exactly the
        // index lookup (NIFTY + BANKNIFTY on IDX_I).
        assert_eq!(combined.len(), PREOPEN_INDEX_UNDERLYINGS.len());
        assert_eq!(
            combined.get(&(13, ExchangeSegment::IdxI)),
            Some(&"NIFTY".to_string())
        );
        assert_eq!(
            combined.get(&(25, ExchangeSegment::IdxI)),
            Some(&"BANKNIFTY".to_string())
        );
    }

    #[test]
    fn test_preopen_first_and_last_minute_constants_match_spec() {
        // 09:08 IST == hour*3600 + min*60
        assert_eq!(PREOPEN_FIRST_MINUTE_SECS_IST, 9 * 3600 + 8 * 60);
        // 09:13 IST (exclusive upper bound — bucket 4 = 09:12:00..09:12:59)
        assert_eq!(PREOPEN_LAST_MINUTE_SECS_IST, 9 * 3600 + 13 * 60);
        // Exactly 5 minutes.
        assert_eq!(
            PREOPEN_LAST_MINUTE_SECS_IST - PREOPEN_FIRST_MINUTE_SECS_IST,
            5 * 60
        );
        assert_eq!(PREOPEN_MINUTE_SLOTS, 5);
    }

    #[test]
    fn test_ist_utc_epoch_helper_is_sane() {
        // Cross-check our test helper: 09:12:00 IST == 03:42:00 UTC on 2026-04-20.
        let ts = ist_utc_epoch(9, 12, 0);
        let dt = chrono::DateTime::<Utc>::from_timestamp(ts, 0).unwrap();
        assert_eq!(dt.hour(), 3);
        assert_eq!(dt.minute(), 42);
        assert_eq!(dt.second(), 0);
    }
}
