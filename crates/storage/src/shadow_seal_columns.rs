//! Candle-engine re-architecture #T1a — pure-function seal row extractor.
//!
//! Translates a [`BufferedSeal`] into the typed row record the
//! `ShadowCandleWriter` passes through the questdb-rs
//! `Buffer::table(...).symbol(...).column_*(...)` chain to land in
//! the appropriate plain `candles_<tf>` table.
//!
//! ## Why a separate pure-function step
//!
//! The questdb-rs `Buffer` does not expose its appended rows for
//! testing. Putting the column-typing logic in a pure function lets us
//! exhaustively test:
//!
//! - timeframe → table name dispatch (all 21 TFs)
//! - segment-code → ILP `symbol` string (all 8 segments + UNKNOWN)
//! - the `bucket_start_ist_secs * 1_000_000_000` IST-nanos conversion
//!   (CRITICAL data-integrity rule — the WS LTT carries IST already,
//!   so we MUST NOT add the +5:30 offset)
//! - integer widening for ILP column types
//!
//! The writer is a thin wrapper that drains the absorption pipeline,
//! calls this extractor, and feeds the `Buffer`.

use tickvault_common::price_precision::round_to_2dp;
use tickvault_common::segment::segment_code_to_str;
use tickvault_trading::candles::BufferedSeal;

/// Typed row record matching the plain `candles_<tf>` DDL columns
/// emitted by [`crate::shadow_persistence::ensure_shadow_candle_tables`]:
///
/// ```sql
/// CREATE TABLE candles_<TF> (
///     segment       SYMBOL,           -- segment_code_to_str
///     security_id   INT,              -- u32 → i64 widening
///     ts            TIMESTAMP,        -- IST nanoseconds
///     open          DOUBLE,
///     high          DOUBLE,
///     low           DOUBLE,
///     close         DOUBLE,
///     volume        LONG,             -- u64 → i64 (saturating)
///     oi            LONG,             -- already i64
///     tick_count    INT               -- u32 → i64 widening
/// ) timestamp(ts) PARTITION BY DAY
///   DEDUP UPSERT KEYS(ts, security_id, segment);
/// ```
///
/// All numeric fields are pre-widened to the questdb-rs ILP API's
/// expected types (`i64` for INT/LONG columns, `f64` for DOUBLE) so
/// the writer's `Buffer::column_i64(...)` / `column_f64(...)` calls
/// receive ready values without per-call conversion arithmetic on the
/// hot path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowSealRow {
    /// Target table: one of the 21 plain `candles_<tf>` table names
    /// from `TfIndex::table_name()`. Used by the writer as
    /// `Buffer::table(table_name)`.
    pub table_name: &'static str,
    /// IST-aligned designated timestamp in nanoseconds.
    /// CRITICAL data-integrity rule (`data-integrity.md`):
    /// `bucket_start_ist_secs` is ALREADY IST (it derives from the
    /// WebSocket LTT) — we MUST NOT add +19_800 offset. We multiply
    /// by `1_000_000_000` only.
    pub timestamp_ist_nanos: i64,
    /// Composite-key part 1 (per I-P1-11). Widened from `u32` to `i64`
    /// for the questdb-rs `column_i64` call.
    pub security_id: i64,
    /// Composite-key part 2 (per I-P1-11). Static string from
    /// `segment_code_to_str` for the questdb-rs `symbol` call. Lands in
    /// the `segment` SYMBOL column.
    pub segment: &'static str,
    /// Broker-source provenance (`"dhan"` / `"groww"`) from `seal.feed.as_str()`.
    /// Lands in the `feed` SYMBOL column, which is part of the candle DEDUP key
    /// `(ts, security_id, segment, feed)` — so a Dhan candle and a Groww candle
    /// for the SAME minute/instrument are BOTH kept (operator lock 2026-06-19).
    /// This is the ONE feed-parameterized seal row — no per-feed forked writer.
    pub feed: &'static str,
    /// `LiveCandleState::open` — already `f64`.
    pub open: f64,
    /// `LiveCandleState::high` — already `f64`.
    pub high: f64,
    /// `LiveCandleState::low` — already `f64`.
    pub low: f64,
    /// `LiveCandleState::close` — already `f64`.
    pub close: f64,
    /// `LiveCandleState::volume` (`u64`) widened with saturation to
    /// `i64`. Volumes ≥ `i64::MAX` would saturate (impossible in
    /// practice).
    pub volume: i64,
    /// `LiveCandleState::oi` — already `i64`.
    pub oi: i64,
    /// `LiveCandleState::tick_count` (`u32`) widened to `i64`.
    pub tick_count: i64,
    /// `LiveCandleState::close_pct_from_prev_day` — already `f64`. The
    /// seal-time price % vs the previous-day close (sourced live from the
    /// tick `close` column). `0.0` pre-market / on day-1 when no prev-day
    /// close is available yet. Lands in the `close_pct_from_prev_day`
    /// DOUBLE column. (oi_pct / volume_pct are intentionally NOT persisted
    /// — spot instruments have no OI and indices no volume, per operator
    /// decision 2026-05-28.)
    pub close_pct_from_prev_day: f64,
    /// `LiveCandleState::open_pct` — already `f64`. The seal-time price %
    /// vs the official 09:15 session open (Quote `day_open` field, the NSE
    /// pre-open auction result). `0.0` pre-market when `session_open` is
    /// still blank. Lands in the `open_pct` DOUBLE column (operator lock
    /// 2026-06-01 §31, Option 2).
    pub open_pct: f64,
    /// `LiveCandleState::change_pct` — already `f64`. The headline day
    /// change % (close vs yesterday's close). Lands in the `change_pct`
    /// DOUBLE column (operator request 2026-06-02).
    pub change_pct: f64,
    /// `LiveCandleState::open_gap_pct` — already `f64`. The opening gap %
    /// (today's 09:15 open vs yesterday's close). Lands in the
    /// `open_gap_pct` DOUBLE column (operator request 2026-06-02).
    pub open_gap_pct: f64,
}

impl ShadowSealRow {
    /// Pure conversion from a [`BufferedSeal`] (post-ring-pop) to the
    /// candle-table row record. `O(1)`, zero allocation.
    ///
    /// Field-by-field:
    /// - `table_name = seal.tf.table_name()` — already-validated
    ///   `&'static str` (no allocation).
    /// - `timestamp_ist_nanos = bucket_start_ist_secs as i64 * 1_000_000_000`
    ///   — CRITICAL: NEVER add IST offset; the WS LTT is already IST.
    /// - `security_id = u32 → i64` — widening cast, never lossy.
    /// - `segment = segment_code_to_str(u8)` — `&'static str`, maps the
    ///   8 known segments + UNKNOWN.
    /// - `volume = u64 → i64` saturating cast (defensive — production
    ///   volumes never approach `i64::MAX`).
    /// - All other numeric fields are pass-through.
    #[inline]
    #[must_use]
    pub fn from_buffered_seal(seal: &BufferedSeal) -> Self {
        let timestamp_ist_nanos =
            i64::from(seal.state.bucket_start_ist_secs).saturating_mul(1_000_000_000);
        let volume_i64 = i64::try_from(seal.state.volume).unwrap_or(i64::MAX);
        Self {
            table_name: seal.tf.table_name(),
            timestamp_ist_nanos,
            // `seal.security_id` is `u64` (2026-06-29 widening); the LONG column
            // is `i64`. Dhan ids fit u32, Groww uses bit 62 (≤ i64::MAX), so this
            // is lossless; `try_from` saturates only a never-produced bit-63 id.
            security_id: i64::try_from(seal.security_id).unwrap_or(i64::MAX),
            segment: segment_code_to_str(seal.exchange_segment_code),
            // Feed provenance from the seal — the ONE writer stamps whatever feed
            // produced the seal (Dhan or Groww), never a hardcoded constant.
            feed: seal.feed.as_str(),
            open: round_to_2dp(seal.state.open),
            high: round_to_2dp(seal.state.high),
            low: round_to_2dp(seal.state.low),
            close: round_to_2dp(seal.state.close),
            volume: volume_i64,
            oi: seal.state.oi,
            tick_count: i64::from(seal.state.tick_count),
            close_pct_from_prev_day: seal.state.close_pct_from_prev_day,
            open_pct: seal.state.open_pct,
            // change_pct == close_pct_from_prev_day (the headline day change
            // %). Derived here rather than stored on LiveCandleState to keep
            // the per-instrument RAM budget (operator request 2026-06-02).
            change_pct: seal.state.close_pct_from_prev_day,
            open_gap_pct: seal.state.open_gap_pct,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::{
        EXCHANGE_SEGMENT_BSE_CURRENCY, EXCHANGE_SEGMENT_BSE_EQ, EXCHANGE_SEGMENT_BSE_FNO,
        EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_MCX_COMM, EXCHANGE_SEGMENT_NSE_CURRENCY,
        EXCHANGE_SEGMENT_NSE_EQ, EXCHANGE_SEGMENT_NSE_FNO,
    };
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    fn mk_seal(sid: u32, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = bucket;
        state.open = 100.0;
        state.high = 105.0;
        state.low = 99.0;
        state.close = close;
        state.volume = 1234;
        state.bucket_start_cumulative = 1000;
        state.oi = 50_000;
        state.tick_count = 5;
        BufferedSeal::new(sid, seg, tf, state, Feed::Dhan)
    }

    #[test]
    fn test_from_buffered_seal_carries_close_pct_from_prev_day() {
        // PR-4b: the seal-time price % must flow from LiveCandleState into
        // the persisted row (it lands in the close_pct_from_prev_day DOUBLE
        // column). 105 close vs 100 prev-day close ⇒ +5.0%.
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.close = 105.0;
        state.prev_day_close = 100.0;
        state.close_pct_from_prev_day = 5.0;
        let seal = BufferedSeal::new(13, EXCHANGE_SEGMENT_NSE_EQ, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert!((row.close_pct_from_prev_day - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_from_buffered_seal_carries_open_pct() {
        // §31 Option 2: the seal-time % vs the official 09:15 open must flow
        // from LiveCandleState into the persisted row (open_pct DOUBLE column).
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.close = 102.5;
        state.session_open = 100.0;
        state.open_pct = 2.5;
        let seal = BufferedSeal::new(13, EXCHANGE_SEGMENT_NSE_EQ, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert!((row.open_pct - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_from_buffered_seal_carries_change_pct_and_open_gap_pct() {
        // Operator request 2026-06-02: change_pct + open_gap_pct must flow
        // from LiveCandleState into the persisted row.
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.close = 105.0;
        state.session_open = 102.0;
        state.prev_day_close = 100.0;
        // change_pct is derived from close_pct_from_prev_day at extraction.
        state.close_pct_from_prev_day = 5.0;
        state.open_gap_pct = 2.0;
        let seal = BufferedSeal::new(13, EXCHANGE_SEGMENT_NSE_EQ, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert!((row.change_pct - 5.0).abs() < f64::EPSILON);
        assert!((row.open_gap_pct - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_from_buffered_seal_change_pct_open_gap_pct_default_zero_premarket() {
        // Pre-market / day-1: no prev-day close ⇒ both pct stay 0.0 (never NaN).
        let row =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
        assert_eq!(row.change_pct, 0.0);
        assert_eq!(row.open_gap_pct, 0.0);
        assert!(!row.change_pct.is_nan());
        assert!(!row.open_gap_pct.is_nan());
    }

    #[test]
    fn test_from_buffered_seal_close_pct_defaults_zero_premarket() {
        // Pre-market / day-1: no prev-day close ⇒ pct stays 0.0 (never NaN).
        let row =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
        assert_eq!(row.close_pct_from_prev_day, 0.0);
        assert!(!row.close_pct_from_prev_day.is_nan());
    }

    #[test]
    fn test_table_name_dispatches_correctly_for_all_21_tfs() {
        for tf in TfIndex::ALL {
            let row = ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, tf, 1_716_000_900, 100.0));
            assert_eq!(
                row.table_name,
                tf.table_name(),
                "table_name mismatch for TF {tf:?}"
            );
        }
    }

    #[test]
    fn test_table_name_canonical_strings() {
        // Pin the EXACT strings so a future refactor of TfIndex doesn't
        // silently change the ILP-emitted table name (which would split
        // candles across two tables — silent data loss class bug).
        let pairs = [
            (TfIndex::M1, "candles_1m"),
            (TfIndex::M2, "candles_2m"),
            (TfIndex::M3, "candles_3m"),
            (TfIndex::M4, "candles_4m"),
            (TfIndex::M5, "candles_5m"),
            (TfIndex::M6, "candles_6m"),
            (TfIndex::M7, "candles_7m"),
            (TfIndex::M8, "candles_8m"),
            (TfIndex::M9, "candles_9m"),
            (TfIndex::M10, "candles_10m"),
            (TfIndex::M11, "candles_11m"),
            (TfIndex::M12, "candles_12m"),
            (TfIndex::M13, "candles_13m"),
            (TfIndex::M14, "candles_14m"),
            (TfIndex::M15, "candles_15m"),
            (TfIndex::M30, "candles_30m"),
            (TfIndex::H1, "candles_1h"),
            (TfIndex::H2, "candles_2h"),
            (TfIndex::H3, "candles_3h"),
            (TfIndex::H4, "candles_4h"),
            (TfIndex::D1, "candles_1d"),
        ];
        for (tf, expected) in pairs {
            let row = ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, tf, 1_716_000_900, 100.0));
            assert_eq!(row.table_name, expected, "TF {tf:?}");
        }
    }

    #[test]
    fn test_table_name_aligns_with_candle_table_names() {
        // Cross-check: the order of TfIndex::ALL matches the order of
        // `shadow_persistence::candle_table_names()`.
        let names = crate::shadow_persistence::candle_table_names();
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            let row = ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, *tf, 1_716_000_900, 100.0));
            assert_eq!(
                row.table_name, names[idx],
                "alignment mismatch at idx {idx}"
            );
        }
    }

    #[test]
    fn test_timestamp_ist_nanos_is_secs_times_billion() {
        // CRITICAL data-integrity rule: bucket_start_ist_secs is
        // already IST (from WS LTT). MUST NOT add +5:30 offset.
        let row =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
        assert_eq!(row.timestamp_ist_nanos, 1_716_000_900_i64 * 1_000_000_000);
    }

    #[test]
    fn test_timestamp_ist_nanos_does_not_apply_ist_offset() {
        // Defensive ratchet: explicitly verify the result does NOT
        // include the +19800 second offset. If a future "helpful"
        // refactor adds the offset, this test fails immediately.
        const SECS_IN_BUCKET: u32 = 1_716_000_900;
        let row =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, SECS_IN_BUCKET, 100.0));
        let with_offset = (i64::from(SECS_IN_BUCKET) + 19_800) * 1_000_000_000;
        assert_ne!(
            row.timestamp_ist_nanos, with_offset,
            "IST offset MUST NOT be added to ts (data-integrity rule)"
        );
    }

    #[test]
    fn test_timestamp_zero_seconds_yields_zero_nanos() {
        let row = ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, 0, 100.0));
        assert_eq!(row.timestamp_ist_nanos, 0);
    }

    #[test]
    fn test_timestamp_max_u32_seconds_saturates_safely() {
        let row = ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, u32::MAX, 100.0));
        assert_eq!(row.timestamp_ist_nanos, i64::from(u32::MAX) * 1_000_000_000);
    }

    #[test]
    fn test_security_id_widens_u32_to_i64() {
        let row = ShadowSealRow::from_buffered_seal(&mk_seal(
            u32::MAX,
            0,
            TfIndex::M1,
            1_716_000_900,
            100.0,
        ));
        assert_eq!(row.security_id, i64::from(u32::MAX));
    }

    #[test]
    fn test_segment_resolves_all_8_known_codes() {
        let cases = [
            (EXCHANGE_SEGMENT_IDX_I, "IDX_I"),
            (EXCHANGE_SEGMENT_NSE_EQ, "NSE_EQ"),
            (EXCHANGE_SEGMENT_NSE_FNO, "NSE_FNO"),
            (EXCHANGE_SEGMENT_NSE_CURRENCY, "NSE_CURRENCY"),
            (EXCHANGE_SEGMENT_BSE_EQ, "BSE_EQ"),
            (EXCHANGE_SEGMENT_MCX_COMM, "MCX_COMM"),
            (EXCHANGE_SEGMENT_BSE_CURRENCY, "BSE_CURRENCY"),
            (EXCHANGE_SEGMENT_BSE_FNO, "BSE_FNO"),
        ];
        for (code, expected) in cases {
            let row = ShadowSealRow::from_buffered_seal(&mk_seal(
                13,
                code,
                TfIndex::M1,
                1_716_000_900,
                100.0,
            ));
            assert_eq!(row.segment, expected, "code {code}");
        }
    }

    #[test]
    fn test_segment_unknown_code_yields_unknown_string() {
        let row =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 99, TfIndex::M1, 1_716_000_900, 100.0));
        assert_eq!(row.segment, "UNKNOWN");
    }

    #[test]
    fn test_volume_widens_u64_to_i64_within_normal_range() {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.volume = 1_000_000_000_u64;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.volume, 1_000_000_000_i64);
    }

    #[test]
    fn test_volume_saturates_when_above_i64_max() {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.volume = u64::MAX;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.volume, i64::MAX);
        assert!(row.volume > 0, "saturated volume MUST stay positive");
    }

    #[test]
    fn test_oi_passes_through_negative_values() {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.oi = -42_000;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.oi, -42_000);
    }

    #[test]
    fn test_tick_count_widens_u32_to_i64() {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.tick_count = u32::MAX;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.tick_count, i64::from(u32::MAX));
    }

    #[test]
    fn test_ohlc_pass_through_unchanged() {
        let row =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 102.5));
        assert_eq!(row.open, 100.0);
        assert_eq!(row.high, 105.0);
        assert_eq!(row.low, 99.0);
        assert_eq!(row.close, 102.5);
    }

    #[test]
    fn test_i_p1_11_segment_isolation_through_extraction() {
        // I-P1-11: same security_id with different segments MUST
        // produce composite-key-distinct rows (different `segment`
        // string). The DEDUP UPSERT KEYS on the candle tables
        // (ts, security_id, segment) rely on this.
        let row_idx =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
        let row_eq =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 1, TfIndex::M1, 1_716_000_900, 200.0));
        assert_eq!(row_idx.security_id, row_eq.security_id);
        assert_ne!(
            row_idx.segment, row_eq.segment,
            "I-P1-11 violation: same security_id different segment must produce different segment strings"
        );
        assert_ne!(row_idx.close, row_eq.close, "row payloads diverge");
    }

    #[test]
    fn test_extraction_is_deterministic_for_identical_inputs() {
        let s = mk_seal(13, 0, TfIndex::H4, 1_716_001_500, 200.75);
        let a = ShadowSealRow::from_buffered_seal(&s);
        let b = ShadowSealRow::from_buffered_seal(&s);
        assert_eq!(a, b);
    }

    #[test]
    fn test_extraction_preserves_every_field_for_full_state() {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_001_500;
        state.open = 11.11;
        state.high = 22.22;
        state.low = 3.33;
        state.close = 44.44;
        state.volume = 1_000_000;
        state.bucket_start_cumulative = 999;
        state.oi = 7_777_777;
        state.tick_count = 42;
        let seal = BufferedSeal::new(99, EXCHANGE_SEGMENT_NSE_FNO, TfIndex::H4, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.table_name, "candles_4h");
        assert_eq!(row.timestamp_ist_nanos, 1_716_001_500_i64 * 1_000_000_000);
        assert_eq!(row.security_id, 99);
        assert_eq!(row.segment, "NSE_FNO");
        assert_eq!(row.open, 11.11);
        assert_eq!(row.high, 22.22);
        assert_eq!(row.low, 3.33);
        assert_eq!(row.close, 44.44);
        assert_eq!(row.volume, 1_000_000);
        assert_eq!(row.oi, 7_777_777);
        assert_eq!(row.tick_count, 42);
    }
}
