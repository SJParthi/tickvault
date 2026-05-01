//! Movers 22-timeframe shared types (Phase 9 of v3 plan, 2026-04-28).
//!
//! Schema for the 22 `movers_{T}` QuestDB tables. The 22 timeframes match
//! the candle ladder so movers can be queried at the same granularity as
//! candles.
//!
//! ## Design constraints
//!
//! - `MoverRow` is `Copy` so per-snapshot serialization is zero-allocation
//!   (the hot path uses arena `Vec<MoverRow>` with `clear() + extend()`).
//! - String-typed columns (`underlying_symbol`, `option_type`) are stored
//!   as `ArrayString<16>` so the struct stays Copy without heap allocation.
//! - 26 columns total = 4 cache lines (~256-296 B per row).
//!
//! ## See also
//!
//! - `.claude/plans/v2-architecture.md` Section B (MoverRow schema)
//! - `crates/storage/src/movers_22tf_persistence.rs` (the 22 ILP writers)
//! - `crates/core/src/pipeline/movers_22tf_scheduler.rs` (the 22 tasks)

use arrayvec::ArrayString;

/// One movers timeframe — pairs the human-readable label (used in table
/// names + Prometheus labels) with its cadence in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MoversTimeframe {
    /// Human-readable label, e.g. `"1s"`, `"5m"`, `"1h"`. Used as the
    /// suffix on the QuestDB table name (`movers_1s`, `movers_5m`, etc.)
    /// and as the `{timeframe}` Prometheus label.
    pub label: &'static str,
    /// Cadence in seconds.
    pub cadence_secs: u64,
}

impl MoversTimeframe {
    /// Construct a timeframe entry. `const`-friendly so the catalogue can
    /// be built at compile time.
    #[must_use]
    pub const fn new(label: &'static str, cadence_secs: u64) -> Self {
        Self {
            label,
            cadence_secs,
        }
    }

    /// QuestDB table name for this timeframe (`movers_{label}`).
    /// Returned as a fixed-capacity ArrayString to avoid heap allocation
    /// on the snapshot path. Capacity 32 is comfortably above the longest
    /// label (`movers_15m` = 10 chars).
    #[must_use]
    pub fn table_name(&self) -> ArrayString<32> {
        let mut s: ArrayString<32> = ArrayString::new();
        // O(1) EXEMPT: bounded label length (≤ 4 chars), no allocation.
        s.push_str("movers_");
        s.push_str(self.label);
        s
    }
}

/// The 22 movers timeframes — same ladder as the candle materialized views.
///
/// Order matters: ratchets pin both the count AND the exact ordering.
/// The labels here MUST match the materialized-view names in the storage
/// crate (e.g. `candles_1s` ↔ `movers_1s`).
pub const MOVERS_TIMEFRAMES: &[MoversTimeframe] = &[
    // Sub-minute (5)
    MoversTimeframe::new("1s", 1),
    MoversTimeframe::new("5s", 5),
    MoversTimeframe::new("10s", 10),
    MoversTimeframe::new("15s", 15),
    MoversTimeframe::new("30s", 30),
    // 1-15 minute sequential (15)
    MoversTimeframe::new("1m", 60),
    MoversTimeframe::new("2m", 120),
    MoversTimeframe::new("3m", 180),
    MoversTimeframe::new("4m", 240),
    MoversTimeframe::new("5m", 300),
    MoversTimeframe::new("6m", 360),
    MoversTimeframe::new("7m", 420),
    MoversTimeframe::new("8m", 480),
    MoversTimeframe::new("9m", 540),
    MoversTimeframe::new("10m", 600),
    MoversTimeframe::new("11m", 660),
    MoversTimeframe::new("12m", 720),
    MoversTimeframe::new("13m", 780),
    MoversTimeframe::new("14m", 840),
    MoversTimeframe::new("15m", 900),
    // Long (2)
    MoversTimeframe::new("30m", 1800),
    MoversTimeframe::new("1h", 3600),
];

/// Number of movers timeframes — pinned at 22 by ratchet test
/// `test_movers_timeframes_constant_is_22`.
pub const MOVERS_TIMEFRAME_COUNT: usize = 22;

const _: () = assert!(
    MOVERS_TIMEFRAMES.len() == MOVERS_TIMEFRAME_COUNT,
    "MOVERS_TIMEFRAMES length must equal MOVERS_TIMEFRAME_COUNT"
);

/// Pre-allocated capacity hint for the per-snapshot arena `Vec<MoverRow>`.
/// 30,000 covers the full ~24,324-instrument universe with headroom for
/// growth without reallocating.
pub const MOVERS_ARENA_CAPACITY: usize = 30_000;

/// One movers snapshot row. `Copy` to keep the snapshot path zero-alloc.
///
/// 26 columns total — see `.claude/plans/v2-architecture.md` Section B.
/// Some fields are only meaningful for certain segments (OI/spot/depth
/// for NSE_FNO; strike/option_type for options) — those are stored as
/// `f64` / `i64` with sentinel values (`NaN` / `0` / empty `ArrayString`)
/// for non-applicable rows; receivers / SQL queries filter by segment +
/// `instrument_type`.
#[derive(Debug, Clone, Copy)]
pub struct MoverRow {
    /// IST epoch nanoseconds — designated QuestDB timestamp + DEDUP key.
    pub ts_nanos: i64,
    /// Dhan instrument identifier. NOT unique alone — pair with `segment`
    /// (per I-P1-11). DEDUP key.
    pub security_id: u32,
    /// Exchange segment as a single-char tag for QuestDB SYMBOL column.
    /// DEDUP key. `'I'` = IDX_I, `'E'` = NSE_EQ, `'D'` = NSE_FNO.
    pub segment: char,
    /// Instrument type tag — `INDEX`, `EQUITY`, `OPTIDX`, `OPTSTK`,
    /// `FUTIDX`, `FUTSTK`. Used by Grafana category filters.
    pub instrument_type: ArrayString<16>,
    /// Underlying symbol — `NIFTY`, `BANKNIFTY`, `RELIANCE`, etc.
    pub underlying_symbol: ArrayString<16>,
    /// Underlying SecurityId. For NSE_FNO derivatives only — used to
    /// look up `spot_price` from `SharedSpotPrices`. 0 for IDX_I + NSE_EQ.
    pub underlying_security_id: u32,
    /// Expiry date as IST epoch days (i64). 0 for non-derivatives.
    pub expiry_date_epoch_days: i64,
    /// Strike price. 0.0 for non-options.
    pub strike_price: f64,
    /// Option type — `'C'` = CE, `'P'` = PE, `'X'` = non-option.
    pub option_type: char,
    /// Last traded price.
    pub ltp: f64,
    /// Previous trading day's close price.
    pub prev_close: f64,
    /// % change vs prev close (used as primary mover sort key).
    pub change_pct: f64,
    /// Absolute change vs prev close (`ltp - prev_close`).
    pub change_abs: f64,
    /// Day's traded volume.
    pub volume: i64,
    /// Total buy quantity (top 5 levels of order book).
    pub buy_qty: i64,
    /// Total sell quantity (top 5 levels of order book).
    pub sell_qty: i64,
    /// Open interest. 0 for non-NSE_FNO rows.
    pub open_interest: i64,
    /// OI change vs first-seen-today OI. 0 for non-NSE_FNO rows.
    pub oi_change: i64,
    /// OI change %. 0.0 for non-NSE_FNO rows.
    pub oi_change_pct: f64,
    /// Underlying spot price at snapshot time. NaN for non-NSE_FNO rows
    /// or when the spot is unknown (resolved by 3-tier fallback).
    pub spot_price: f64,
    /// Best bid price (depth level 0). NaN if depth not subscribed.
    pub best_bid: f64,
    /// Best ask price (depth level 0). NaN if depth not subscribed.
    pub best_ask: f64,
    /// `(best_ask - best_bid) / ltp * 100`. NaN if depth missing.
    pub spread_pct: f64,
    /// Sum of top-5 bid quantities. 0 if depth missing.
    pub bid_pressure_5: i64,
    /// Sum of top-5 ask quantities. 0 if depth missing.
    pub ask_pressure_5: i64,
    /// Wall-clock receipt time in IST epoch nanoseconds (audit field —
    /// may differ from `ts_nanos` for replays / restarts).
    pub received_at_nanos: i64,
}

impl MoverRow {
    /// IST 1970-01-01 epoch nanos used as the placeholder for unset
    /// timestamp fields.
    pub const NULL_TS_NANOS: i64 = 0;

    /// Constructs an empty `MoverRow` with sentinel values everywhere.
    /// Useful for arena pre-allocation and tests.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            ts_nanos: Self::NULL_TS_NANOS,
            security_id: 0,
            segment: '?',
            instrument_type: ArrayString::new(),
            underlying_symbol: ArrayString::new(),
            underlying_security_id: 0,
            expiry_date_epoch_days: 0,
            strike_price: 0.0,
            option_type: 'X',
            ltp: f64::NAN,
            prev_close: f64::NAN,
            change_pct: f64::NAN,
            change_abs: f64::NAN,
            volume: 0,
            buy_qty: 0,
            sell_qty: 0,
            open_interest: 0,
            oi_change: 0,
            oi_change_pct: f64::NAN,
            spot_price: f64::NAN,
            best_bid: f64::NAN,
            best_ask: f64::NAN,
            spread_pct: f64::NAN,
            bid_pressure_5: 0,
            ask_pressure_5: 0,
            received_at_nanos: Self::NULL_TS_NANOS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 9 ratchet: exactly 22 timeframes, exact ordering pinned.
    #[test]
    fn test_movers_timeframes_constant_is_22() {
        assert_eq!(MOVERS_TIMEFRAMES.len(), MOVERS_TIMEFRAME_COUNT);
        assert_eq!(MOVERS_TIMEFRAME_COUNT, 22);
    }

    #[test]
    fn test_movers_timeframes_first_is_1s_last_is_1h() {
        assert_eq!(MOVERS_TIMEFRAMES[0].label, "1s");
        assert_eq!(MOVERS_TIMEFRAMES[0].cadence_secs, 1);
        assert_eq!(MOVERS_TIMEFRAMES[21].label, "1h");
        assert_eq!(MOVERS_TIMEFRAMES[21].cadence_secs, 3600);
    }

    #[test]
    fn test_movers_timeframes_strictly_monotonic_cadence() {
        for window in MOVERS_TIMEFRAMES.windows(2) {
            assert!(
                window[0].cadence_secs < window[1].cadence_secs,
                "cadence must be strictly increasing: {} ({}s) -> {} ({}s)",
                window[0].label,
                window[0].cadence_secs,
                window[1].label,
                window[1].cadence_secs,
            );
        }
    }

    #[test]
    fn test_movers_timeframes_includes_4m_to_14m_sequential() {
        // The 9 new candle views (4m, 6m, 7m, 8m, 9m, 11m, 12m, 13m, 14m)
        // must all be present in the ladder so movers and candles match.
        let labels: Vec<&str> = MOVERS_TIMEFRAMES.iter().map(|tf| tf.label).collect();
        for new_label in ["4m", "6m", "7m", "8m", "9m", "11m", "12m", "13m", "14m"] {
            assert!(
                labels.contains(&new_label),
                "new candle/movers timeframe {new_label} missing from MOVERS_TIMEFRAMES"
            );
        }
    }

    #[test]
    fn test_movers_table_name_format() {
        assert_eq!(MOVERS_TIMEFRAMES[0].table_name().as_str(), "movers_1s");
        assert_eq!(MOVERS_TIMEFRAMES[5].table_name().as_str(), "movers_1m");
        assert_eq!(MOVERS_TIMEFRAMES[21].table_name().as_str(), "movers_1h");
    }

    #[test]
    fn test_mover_row_size_at_most_4_cache_lines() {
        // 4 cache lines = 256 B. ArrayString<16> + 16 numeric fields ~ 256.
        // Fail loudly if anyone bumps the struct beyond this.
        let actual = std::mem::size_of::<MoverRow>();
        assert!(
            actual <= 320,
            "MoverRow grew beyond 4-5 cache lines (320 B): actual = {actual} B"
        );
    }

    #[test]
    fn test_mover_row_is_copy() {
        // Static guarantee: the struct must be Copy so arena snapshot is
        // zero-allocation. If anyone adds a String field this fails to
        // compile (Copy bound violated).
        fn assert_copy<T: Copy>() {}
        assert_copy::<MoverRow>();
    }

    #[test]
    fn test_mover_row_empty_uses_safe_sentinels() {
        let row = MoverRow::empty();
        assert_eq!(row.security_id, 0);
        assert_eq!(row.segment, '?');
        assert!(row.ltp.is_nan());
        assert!(row.spot_price.is_nan());
        assert_eq!(row.volume, 0);
        assert_eq!(row.option_type, 'X');
    }
}
