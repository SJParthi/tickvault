//! `LiveCandleState` — the shared per-bucket OHLCV state struct.
//!
//! Extracted 2026-07-17 (stage-3 dead-WS sweep) from the DELETED
//! `aggregator_cell.rs` (the publisher-less 21-TF TICK aggregator died
//! with the live-feed retirements — Dhan 2026-07-13, Groww 2026-07-15).
//! The struct itself is load-bearing across the SURVIVING seal chain:
//! it is the payload of [`crate::candles::BufferedSeal`], consumed by the
//! storage seal-writer chain (`seal_writer_loop` / `ShadowCandleWriter` /
//! spill / DLQ) and PRODUCED today only by the REST-era candle fold
//! (`crates/app/src/rest_candle_fold.rs` — FOLD-01), which constructs it
//! from official `spot_1m_rest` bars. The tick-fold constructors
//! (`from_first_tick` / `fold_in_bucket` / `fold_late_hlc`) died with the
//! aggregator; construction is now literal-field (all fields `pub`).
//!
//! Field semantics are UNCHANGED from the deleted cell (the QuestDB
//! `candles_<tf>` column contract depends on them — see
//! `shadow_seal_columns.rs`).

/// Per-bucket live candle state (one open bucket of one timeframe).
///
/// The 3 Wave-5 pct fields plus `open_pct` / `open_gap_pct` stay `0.0`
/// until the seal-time writer stamps them via
/// [`crate::candles::stamp_seal_pct_fields`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveCandleState {
    /// Bucket-open IST epoch second (aligned to TF boundary).
    /// `0` means "slot never opened" — the empty/initial state.
    pub bucket_start_ist_secs: u32,
    /// Open price of this bucket.
    pub open: f64,
    /// Running high.
    pub high: f64,
    /// Running low.
    pub low: f64,
    /// Close (last folded price).
    pub close: f64,
    /// **Incremental** volume within this bucket.
    pub volume: u64,
    /// Cumulative-volume snapshot at bucket-open. Set ONCE per bucket
    /// open; retained for column-contract compatibility.
    pub bucket_start_cumulative: u64,
    /// Open Interest snapshot from the latest fold.
    pub oi: i64,
    /// Number of source rows/ticks folded into this bucket.
    pub tick_count: u32,
    /// IST epoch secs of the fold that set the current `close`.
    pub close_ts_ist_secs: u32,
    /// Previous-day close baseline (last non-zero value wins — a blank
    /// pre-market `0` never clobbers a real baseline). Feeds
    /// `close_pct_from_prev_day` at seal.
    pub prev_day_close: f64,
    /// `close - prev_day.close` / `prev_day.close` * 100.0. Stamped at
    /// seal time.
    pub close_pct_from_prev_day: f64,
    /// `oi - prev_day.oi` / `prev_day.oi` * 100.0. Stamped at seal time.
    pub oi_pct_from_prev_day: f64,
    /// `volume / prev_day.volume * 100.0`. Stamped at seal time.
    pub volume_pct_from_prev_day: f64,
    /// Today's SESSION open (the official 09:15 open). Static per trading
    /// day; last non-zero value wins. Feeds `open_pct` at seal.
    pub session_open: f64,
    /// `(close - session_open) / session_open * 100.0` — % change vs the
    /// official 09:15 open. Stamped at seal time. `0.0` if `session_open`
    /// is `0.0` (div-by-zero guard).
    pub open_pct: f64,
    /// `(session_open - prev_day_close) / prev_day_close * 100.0` — the
    /// OPENING GAP % (gap-up positive, gap-down negative). Stamped at
    /// seal time. `0.0` if `prev_day_close` is `0.0` (div-by-zero guard).
    pub open_gap_pct: f64,
}

impl LiveCandleState {
    /// Empty/initial state — `bucket_start_ist_secs == 0` flags the
    /// "never opened" sentinel.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            bucket_start_ist_secs: 0,
            open: 0.0,
            high: f64::NEG_INFINITY,
            low: f64::INFINITY,
            close: 0.0,
            volume: 0,
            bucket_start_cumulative: 0,
            oi: 0,
            tick_count: 0,
            close_ts_ist_secs: 0,
            prev_day_close: 0.0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
            session_open: 0.0,
            open_pct: 0.0,
            open_gap_pct: 0.0,
        }
    }

    /// Returns `true` if this slot has never been folded into (the
    /// boot/empty state). [`Self::bucket_start_ist_secs`] is the cheap
    /// check.
    #[inline]
    #[must_use]
    pub const fn is_uninitialised(&self) -> bool {
        self.bucket_start_ist_secs == 0
    }
}

#[cfg(test)]
mod tests {
    use super::LiveCandleState;

    #[test]
    fn test_empty_is_uninitialised_sentinel() {
        let s = LiveCandleState::empty();
        assert!(s.is_uninitialised());
        assert_eq!(s.bucket_start_ist_secs, 0);
        assert_eq!(s.tick_count, 0);
        assert_eq!(s.volume, 0);
        // Extreme sentinels so the first fold's min/max always win.
        assert!(s.high.is_infinite() && s.high < 0.0);
        assert!(s.low.is_infinite() && s.low > 0.0);
    }

    #[test]
    fn test_opened_state_is_not_uninitialised() {
        let s = LiveCandleState {
            bucket_start_ist_secs: 33_300, // 09:15:00 IST secs-of-day-shaped value
            ..LiveCandleState::empty()
        };
        assert!(!s.is_uninitialised());
    }
}
