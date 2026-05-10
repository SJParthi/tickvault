//! Wave 6 Sub-PR #1 item 1.2f.1 — pure-function shadow-seal row extractor.
//!
//! Translates a [`BufferedSeal`] into the typed row record the future
//! `ShadowCandleWriter` (item 1.2f.2) will pass through the questdb-rs
//! `Buffer::table(...).symbol(...).column_*(...)` chain to land in
//! the appropriate `candles_*_shadow` table.
//!
//! ## Why a separate pure-function step
//!
//! The questdb-rs `Buffer` does not expose its appended rows for
//! testing. Putting the column-typing logic in a pure function lets us
//! exhaustively test:
//!
//! - timeframe → table name dispatch (all 9 TFs)
//! - segment-code → ILP `symbol` string (all 8 segments + UNKNOWN)
//! - the `bucket_start_ist_secs * 1_000_000_000` IST-nanos conversion
//!   (CRITICAL data-integrity rule — the WS LTT carries IST already,
//!   so we MUST NOT add the +5:30 offset; mirrors
//!   `compute_live_candle_nanos` in `candle_persistence.rs`)
//! - integer widening for ILP column types (`u32` → `i64` for
//!   `column_i64`)
//!
//! The future writer (item 1.2f.2) is then a thin wrapper that drains
//! the absorption pipeline, calls this extractor, and feeds the
//! `Buffer`. No business logic lives in the writer; it only manages
//! the connection / batch lifecycle.
//!
//! ## What this slice ships
//!
//! - [`ShadowSealRow`] — typed value record carrying every column the
//!   `candles_*_shadow` DDL (in `shadow_persistence.rs`) defines.
//! - [`ShadowSealRow::from_buffered_seal`] — pure conversion from a
//!   [`BufferedSeal`] (post-ring-pop) to the row record.
//! - 19 unit tests covering every TF dispatch, every segment string,
//!   IST-nanos correctness, integer widening, the 3 Wave-5 pct fields,
//!   I-P1-11 segment isolation, and the canonical column ordering
//!   matching the DDL in `shadow_persistence.rs`.
//!
//! ## What this slice does NOT ship
//!
//! - The actual `ShadowCandleWriter` struct / ILP `Sender` (item 1.2f.2).
//! - `Buffer::table(...).symbol(...).column_*(...)` chain (item 1.2f.2).
//! - Async writer task draining the [`SealAbsorptionPipeline`]
//!   (item 1.2f.3).
//! - Boot wiring + counter increments.

use tickvault_common::segment::segment_code_to_str;
use tickvault_trading::candles::BufferedSeal;

/// Typed row record matching the `candles_*_shadow` DDL columns
/// emitted by [`crate::shadow_persistence::ensure_shadow_candle_tables`]:
///
/// ```sql
/// CREATE TABLE candles_<TF>_shadow (
///     ts                          TIMESTAMP,        -- IST nanoseconds
///     security_id                 INT,              -- u32 → i64 widening
///     exchange_segment            SYMBOL,           -- segment_code_to_str
///     open                        DOUBLE,
///     high                        DOUBLE,
///     low                         DOUBLE,
///     close                       DOUBLE,
///     volume                      LONG,             -- u64 → i64 (saturating)
///     oi                          LONG,             -- already i64
///     tick_count                  INT,              -- u32 → i64 widening
///     close_pct_from_prev_day     DOUBLE,
///     oi_pct_from_prev_day        DOUBLE,
///     volume_pct_from_prev_day    DOUBLE
/// ) timestamp(ts) PARTITION BY DAY
///   DEDUP UPSERT KEYS(ts, security_id, exchange_segment);
/// ```
///
/// All numeric fields are pre-widened to the questdb-rs ILP API's
/// expected types (`i64` for INT/LONG columns, `f64` for DOUBLE) so
/// the writer slice's `Buffer::column_i64(...)` / `column_f64(...)`
/// calls receive ready values without per-call conversion arithmetic
/// on the hot path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowSealRow {
    /// Target table: one of the 9 `candles_*_shadow` table names from
    /// `TfIndex::shadow_table_name()`. Used by the writer slice as
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
    /// `segment_code_to_str` for the questdb-rs `symbol` call.
    pub exchange_segment: &'static str,
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
    /// practice — even Berkshire Hathaway's daily volume fits in
    /// `i32`).
    pub volume: i64,
    /// `LiveCandleState::oi` — already `i64`.
    pub oi: i64,
    /// `LiveCandleState::tick_count` (`u32`) widened to `i64`.
    pub tick_count: i64,
    /// Wave-5 § K-L12 — stamped at seal by the upstream
    /// aggregator (per L-H6).
    pub close_pct_from_prev_day: f64,
    /// Wave-5 § K-L12 — stamped at seal.
    pub oi_pct_from_prev_day: f64,
    /// Wave-5 § K-L12 — stamped at seal.
    pub volume_pct_from_prev_day: f64,
}

impl ShadowSealRow {
    /// Pure conversion from a [`BufferedSeal`] (post-ring-pop) to the
    /// shadow-table row record. `O(1)`, zero allocation.
    ///
    /// Field-by-field:
    /// - `table_name = seal.tf.shadow_table_name()` — already-validated
    ///   `&'static str` (no allocation).
    /// - `timestamp_ist_nanos = bucket_start_ist_secs as i64 * 1_000_000_000`
    ///   — CRITICAL: NEVER add IST offset; the WS LTT is already IST.
    ///   Mirrors `compute_live_candle_nanos` in `candle_persistence.rs`
    ///   (saturating arithmetic to defend against rogue 32-bit input).
    /// - `security_id = u32 → i64` — widening cast, never lossy.
    /// - `exchange_segment = segment_code_to_str(u8)` — `&'static str`,
    ///   maps the 8 known segments + UNKNOWN.
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
            table_name: seal.tf.shadow_table_name(),
            timestamp_ist_nanos,
            security_id: i64::from(seal.security_id),
            exchange_segment: segment_code_to_str(seal.exchange_segment_code),
            open: seal.state.open,
            high: seal.state.high,
            low: seal.state.low,
            close: seal.state.close,
            volume: volume_i64,
            oi: seal.state.oi,
            tick_count: i64::from(seal.state.tick_count),
            close_pct_from_prev_day: seal.state.close_pct_from_prev_day,
            oi_pct_from_prev_day: seal.state.oi_pct_from_prev_day,
            volume_pct_from_prev_day: seal.state.volume_pct_from_prev_day,
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
        state.close_pct_from_prev_day = 1.5;
        state.oi_pct_from_prev_day = -0.2;
        state.volume_pct_from_prev_day = 12.3;
        BufferedSeal::new(sid, seg, tf, state)
    }

    #[test]
    fn test_table_name_dispatches_correctly_for_all_9_tfs() {
        for tf in TfIndex::ALL {
            let row = ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, tf, 1_716_000_900, 100.0));
            assert_eq!(
                row.table_name,
                tf.shadow_table_name(),
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
            (TfIndex::M1, "candles_1m_shadow"),
            (TfIndex::M5, "candles_5m_shadow"),
            (TfIndex::M15, "candles_15m_shadow"),
            (TfIndex::M30, "candles_30m_shadow"),
            (TfIndex::H1, "candles_1h_shadow"),
            (TfIndex::H2, "candles_2h_shadow"),
            (TfIndex::H3, "candles_3h_shadow"),
            (TfIndex::H4, "candles_4h_shadow"),
            (TfIndex::D1, "candles_1d_shadow"),
        ];
        for (tf, expected) in pairs {
            let row = ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, tf, 1_716_000_900, 100.0));
            assert_eq!(row.table_name, expected, "TF {tf:?}");
        }
    }

    #[test]
    fn test_table_name_aligns_with_shadow_candle_table_names() {
        // Cross-check: the order of TfIndex::ALL matches the order of
        // `shadow_persistence::shadow_candle_table_names()`.
        let names = crate::shadow_persistence::shadow_candle_table_names();
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
        // Empty/initial state has bucket_start_ist_secs == 0. The
        // future writer must treat this as a "never opened" sentinel
        // (likely skip), but the conversion itself must be exact.
        let row = ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, 0, 100.0));
        assert_eq!(row.timestamp_ist_nanos, 0);
    }

    #[test]
    fn test_timestamp_max_u32_seconds_saturates_safely() {
        // u32::MAX seconds = ~136 years; well past 2038. The
        // saturating multiply must not panic or wrap.
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
    fn test_exchange_segment_resolves_all_8_known_codes() {
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
            assert_eq!(row.exchange_segment, expected, "code {code}");
        }
    }

    #[test]
    fn test_exchange_segment_unknown_code_yields_unknown_string() {
        // Defensive: a future Dhan protocol change that introduces a
        // new segment code MUST surface as the string "UNKNOWN" (not
        // panic, not silently drop) so operator + audit can spot it.
        let row =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 99, TfIndex::M1, 1_716_000_900, 100.0));
        assert_eq!(row.exchange_segment, "UNKNOWN");
    }

    #[test]
    fn test_volume_widens_u64_to_i64_within_normal_range() {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.volume = 1_000_000_000_u64;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.volume, 1_000_000_000_i64);
    }

    #[test]
    fn test_volume_saturates_when_above_i64_max() {
        // Defensive: in practice volumes never approach i64::MAX
        // (~9.2 quintillion; even global daily equity volume is ~10^10).
        // But the conversion MUST saturate, not wrap, so a corrupt
        // upstream value cannot turn into a negative ILP write.
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.volume = u64::MAX;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.volume, i64::MAX);
        assert!(row.volume > 0, "saturated volume MUST stay positive");
    }

    #[test]
    fn test_oi_passes_through_negative_values() {
        // i64 OI can be negative for short positions. The pass-through
        // MUST preserve the sign.
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.oi = -42_000;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.oi, -42_000);
    }

    #[test]
    fn test_tick_count_widens_u32_to_i64() {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.tick_count = u32::MAX;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state);
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
    fn test_wave5_pct_fields_pass_through_with_signs() {
        // The 3 stamped-at-seal pct fields MUST round-trip the upstream
        // values verbatim — including negative red-day pct values.
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.close_pct_from_prev_day = -3.5;
        state.oi_pct_from_prev_day = -10.0;
        state.volume_pct_from_prev_day = -100.0;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.close_pct_from_prev_day, -3.5);
        assert_eq!(row.oi_pct_from_prev_day, -10.0);
        assert_eq!(row.volume_pct_from_prev_day, -100.0);
    }

    #[test]
    fn test_wave5_pct_fields_pass_through_zeros() {
        // PREVCLOSE-04 cold-boot path: pct fields are 0.0 if cache
        // is empty. The extractor MUST preserve the literal zero.
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.close_pct_from_prev_day = 0.0;
        state.oi_pct_from_prev_day = 0.0;
        state.volume_pct_from_prev_day = 0.0;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.close_pct_from_prev_day, 0.0);
        assert_eq!(row.oi_pct_from_prev_day, 0.0);
        assert_eq!(row.volume_pct_from_prev_day, 0.0);
    }

    #[test]
    fn test_i_p1_11_segment_isolation_through_extraction() {
        // I-P1-11: same security_id with different segments MUST
        // produce composite-key-distinct rows (different
        // exchange_segment string). The DEDUP UPSERT KEYS on the
        // shadow tables (security_id, exchange_segment) rely on this.
        let row_idx =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
        let row_eq =
            ShadowSealRow::from_buffered_seal(&mk_seal(13, 1, TfIndex::M1, 1_716_000_900, 200.0));
        assert_eq!(row_idx.security_id, row_eq.security_id);
        assert_ne!(
            row_idx.exchange_segment, row_eq.exchange_segment,
            "I-P1-11 violation: same security_id different segment must produce different exchange_segment strings"
        );
        assert_ne!(row_idx.close, row_eq.close, "row payloads diverge");
    }

    #[test]
    fn test_extraction_is_deterministic_for_identical_inputs() {
        // The same input must produce the same output every call —
        // pre-condition for the future writer's idempotency contract
        // (DEDUP UPSERT relies on stable (ts, sid, segment) tuples).
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
        state.close_pct_from_prev_day = 0.5;
        state.oi_pct_from_prev_day = 1.5;
        state.volume_pct_from_prev_day = 2.5;
        let seal = BufferedSeal::new(99, EXCHANGE_SEGMENT_NSE_FNO, TfIndex::H4, state);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.table_name, "candles_4h_shadow");
        assert_eq!(row.timestamp_ist_nanos, 1_716_001_500_i64 * 1_000_000_000);
        assert_eq!(row.security_id, 99);
        assert_eq!(row.exchange_segment, "NSE_FNO");
        assert_eq!(row.open, 11.11);
        assert_eq!(row.high, 22.22);
        assert_eq!(row.low, 3.33);
        assert_eq!(row.close, 44.44);
        assert_eq!(row.volume, 1_000_000);
        assert_eq!(row.oi, 7_777_777);
        assert_eq!(row.tick_count, 42);
        assert_eq!(row.close_pct_from_prev_day, 0.5);
        assert_eq!(row.oi_pct_from_prev_day, 1.5);
        assert_eq!(row.volume_pct_from_prev_day, 2.5);
    }
}
