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
//! - timeframe → table name dispatch (all 12 TFs)
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

/// Typed row record for the `candles_<tf>` tables written by
/// [`crate::shadow_persistence::ensure_shadow_candle_tables`].
///
/// **The DDL is deliberately NOT reproduced here.** It used to be, as a
/// twenty-line SQL sketch, and by 2026-08-22 that sketch was wrong in four
/// separate ways at once: it omitted the `feed` column entirely, it still
/// showed `security_id INT` from before the u64 widening, it showed
/// `tick_count INT`, and it predated the `*_pct_from_prev_day` columns. Worst
/// of it, the sketch stated the dedup key as `(ts, security_id, segment)` —
/// no `feed` — while a doc comment thirty lines below it in this same file
/// correctly stated `(ts, security_id, segment, feed)`.
///
/// That is not a cosmetic problem. A dedup key without `feed` means a Dhan
/// candle and a Groww candle for the same instrument and minute collide, and
/// one silently overwrites the other. A reader trusting the sketch would
/// conclude the system had exactly that bug. An automated sweep on
/// 2026-08-22 flagged precisely this line as a suspected live defect and
/// cost real time proving it was only the comment that was wrong — the same
/// way a stale row in CLAUDE.md's own complexity table manufactured a false
/// finding on 2026-08-12.
///
/// So this doc cites SYMBOLS instead of copying values, because a symbol
/// survives every edit above it and a copied snapshot does not:
///
/// - column list and types → `ensure_shadow_candle_tables`
/// - dedup key → [`crate::shadow_persistence::DEDUP_KEY_CANDLES`]
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
    /// The bar's **SIGNED GROSS VOLUME** — every unit that traded in the
    /// bucket, carrying the bar's direction in its sign.
    ///
    /// Read from [`LiveCandleState::signed_volume`], which owns the whole
    /// rule: negative when this bar's `close` is BELOW the previous bar's
    /// close of the same timeframe, positive otherwise. The MAGNITUDE is the
    /// unaltered gross volume, so `abs(volume)` is exactly what this column
    /// held before it was signed.
    ///
    /// ⚠ **Three separate cases print POSITIVE and none of them means "the
    /// close rose"**: a flat bar, the session's first bucket (no previous
    /// close), and a previous close that cannot be ordered. Positive is the
    /// default, not an assertion.
    ///
    /// ⚠ **This is NOT net order flow.** A bar that traded 1,000 into the bid
    /// and 900 into the offer and closed one tick up reports `+1_900`, not
    /// `-100`. The tick-rule flow value is no longer persisted anywhere —
    /// the trade, and the operator ruling behind it, are recorded in
    /// `websocket-connection-scope-lock.md` § "2026-09-18 (FOURTH)".
    ///
    /// Widened from the state's `u64` with saturation at `i64::MAX`
    /// (impossible in practice; a saturating read stays a plausible
    /// "impossibly large" rather than wrapping to a fake sell bar).
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
    /// `LiveCandleState::total_buy_qty` (`u32`) widened to `i64`. The vendor's
    /// total PENDING BUY-order quantity resting in the book at this bar's last
    /// observed packet — **not executed volume**. `0` is the vendor's ABSENT
    /// sentinel (a Ticker-mode packet carries no book), and is persisted as
    /// `0` rather than NULL because the fold already applied the
    /// last-non-zero rule: a `0` here means the whole bar saw no book.
    pub total_buy_qty: i64,
    /// `LiveCandleState::total_sell_qty` (`u32`) widened to `i64`. Same
    /// semantics as `total_buy_qty` — pending SELL orders, not trades.
    pub total_sell_qty: i64,
    /// Nanoseconds between this bar's window OPENING and the first tick of it
    /// that this process RECEIVED. `None` when the bucket carries no receipt
    /// clock at all — a pre-`TVW3` WAL replay, or a seal that went to disk and
    /// came back (the 128-byte spill record is full and does not carry the
    /// stamps, so a replayed seal reports unknown rather than a fabricated 0).
    ///
    /// The writer renders BOTH halves of the pair from this one figure:
    /// `open_latency` (whole units, human-readable) and `open_latency_ns`
    /// (exact, the only one that may be sorted on). `None` writes NEITHER —
    /// an empty cell is the honest rendering of an unknown delay, where
    /// `0 nanoseconds` would claim the fastest possible delivery.
    ///
    /// ⚠ This MEASURES receipt against the window; it never BUCKETS by it.
    /// `ts` is the exchange clock and nothing else decides which bar a trade
    /// enters (`fold_clock_ist_secs`, the 2026-09-18 SECOND directive).
    pub open_latency_ns: Option<i64>,
    /// Nanoseconds between the last tick of this bar that this process
    /// RECEIVED and the window CLOSING. Same `None` contract and same
    /// measure-never-bucket rule as [`Self::open_latency_ns`].
    pub close_latency_ns: Option<i64>,
    /// Nanoseconds from the first RECEIVED tick of this bar to the last —
    /// the receipt window the bar's data actually occupied. `Some(0)` is a
    /// real reading (a single-tick bucket has a zero span) and is rendered
    /// `0 nanoseconds`, which is why this is an `Option` rather than a
    /// sentinel: the unknown case and the genuinely-instant case must not
    /// share a value.
    pub window_span_latency_ns: Option<i64>,
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
    /// - `volume = LiveCandleState::signed_volume()` — the gross volume
    ///   carrying the bar's direction in its sign (2026-09-18 directive).
    /// - All other numeric fields are pass-through.
    #[inline]
    #[must_use]
    pub fn from_buffered_seal(seal: &BufferedSeal) -> Self {
        let timestamp_ist_nanos =
            i64::from(seal.state.bucket_start_ist_secs).saturating_mul(1_000_000_000);
        let volume_i64 = seal.state.signed_volume();
        // The three delays, derived HERE rather than stored on
        // `LiveCandleState`: the fold keeps the two receipt STAMPS (16 bytes,
        // multiplied by TF_COUNT and by AGGREGATOR_MAX_SLOTS), and the window
        // edges they are measured against are already known from the bucket
        // start and the frame's own period. Storing three more derived figures
        // per open bucket would pay fleet RAM for arithmetic that costs
        // nothing at seal time.
        //
        // `0` on a stamp is the "no receipt" sentinel, so it maps to `None`
        // and the writer emits NEITHER half of that pair. A seal that was
        // spilled to disk and replayed arrives here with both stamps at `0`
        // for exactly that reason — the 128-byte spill record is full and does
        // not carry them, so a replayed bar reports its delays as unknown
        // instead of fabricating an instant one.
        let window_open_ist_nanos = timestamp_ist_nanos;
        let window_close_ist_nanos = window_open_ist_nanos
            .saturating_add(i64::from(seal.tf.seconds_per_bucket()).saturating_mul(1_000_000_000));
        let first_receipt = seal.state.first_receipt_ist_nanos;
        let last_receipt = seal.state.last_receipt_ist_nanos;
        let open_latency_ns =
            (first_receipt > 0).then(|| first_receipt.saturating_sub(window_open_ist_nanos));
        let close_latency_ns =
            (last_receipt > 0).then(|| window_close_ist_nanos.saturating_sub(last_receipt));
        // Both stamps required: a span needs two real edges, and the fold
        // seeds them together, so one present without the other cannot happen
        // — the test is belt-and-braces rather than a case being handled.
        let window_span_latency_ns = (first_receipt > 0 && last_receipt > 0)
            .then(|| last_receipt.saturating_sub(first_receipt));
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
            total_buy_qty: i64::from(seal.state.total_buy_qty),
            total_sell_qty: i64::from(seal.state.total_sell_qty),
            open_latency_ns,
            close_latency_ns,
            window_span_latency_ns,
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
    use tickvault_trading::candles::{LiveCandleState, TF_COUNT, TfIndex};

    fn mk_seal(sid: u64, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
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
    fn test_table_name_dispatches_correctly_for_every_frame() {
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
        // 2026-09-19 nine-frame collapse. Two things this list gets right
        // that the 21-entry version it replaces did not:
        //   - it is COMPLETE. The old list pinned 21 of the then-24 frames;
        //     M2, M30 and M60 had no entry at all, so their table names were
        //     unpinned while the test's name claimed otherwise.
        //   - the length is asserted against TF_COUNT below, so a frame added
        //     without a pin fails the build instead of going unnoticed.
        let pairs = [
            (TfIndex::M1, "candles_1m"),
            (TfIndex::M3, "candles_3m"),
            (TfIndex::M5, "candles_5m"),
            (TfIndex::M15, "candles_15m"),
            (TfIndex::S1, "candles_1s"),
            (TfIndex::S3, "candles_3s"),
            (TfIndex::S5, "candles_5s"),
            (TfIndex::M30, "candles_30m"),
            (TfIndex::M60, "candles_60m"),
        ];
        assert_eq!(
            pairs.len(),
            TF_COUNT,
            "every frame must have a pinned table name"
        );
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
            u64::from(u32::MAX),
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
        // The baseline is set ABOVE the close DELIBERATELY. With the
        // `LiveCandleState::empty()` default of `0.0` this test was VACUOUS:
        // `signed_volume` returns early on `prev <= 0.0`, so the old
        // "saturated volume MUST stay positive" assertion was satisfied by
        // the no-baseline rule and could not fail whatever the saturation
        // arm did. Driving the NEGATING arm is what actually exercises it.
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.volume = u64::MAX;
        state.close = 100.0;
        state.bucket_open_prev_close = 101.0;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(
            row.volume,
            -i64::MAX,
            "a saturated magnitude must still take the bar's sign, and the \
             negation must not overflow: -i64::MAX is i64::MIN + 1"
        );
        assert_ne!(
            row.volume,
            i64::MIN,
            "i64::MIN is unreachable by construction; reaching it would mean \
             the saturation used wrapping arithmetic"
        );
        // And the same bar with a RISING close keeps the positive saturation.
        let mut up = LiveCandleState::empty();
        up.bucket_start_ist_secs = 1_716_000_900;
        up.volume = u64::MAX;
        up.close = 102.0;
        up.bucket_open_prev_close = 101.0;
        let up_seal = BufferedSeal::new(13, 0, TfIndex::M1, up, Feed::Dhan);
        assert_eq!(ShadowSealRow::from_buffered_seal(&up_seal).volume, i64::MAX);
    }

    #[test]
    fn the_row_carries_the_sign_not_just_the_magnitude() {
        // The 2026-09-18 directive: the persisted `volume` is the bar's own
        // gross volume carrying its direction. A row built from a bar that
        // closed BELOW the previous bar of the same timeframe must reach the
        // writer already negative — the conversion is the only place the sign
        // is applied, so a regression here silently un-signs every candle.
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.volume = 900;
        state.bucket_open_prev_close = 101.0;
        state.close = 100.0;
        let seal = BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan);
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.volume, -900, "a down bar signs its whole gross volume");
        assert_eq!(
            row.volume.unsigned_abs(),
            state.volume,
            "the magnitude must survive the sign — `abs()` is what the 10m \
             derivation sums, so a lost magnitude breaks it silently"
        );
    }

    #[test]
    fn an_up_bar_and_a_flat_bar_both_reach_the_row_positive() {
        for (prev, close) in [(99.0_f64, 100.0_f64), (100.0, 100.0)] {
            let mut state = LiveCandleState::empty();
            state.bucket_start_ist_secs = 1_716_000_900;
            state.volume = 900;
            state.bucket_open_prev_close = prev;
            state.close = close;
            let seal = BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan);
            let row = ShadowSealRow::from_buffered_seal(&seal);
            assert_eq!(
                row.volume, 900,
                "prev={prev} close={close}: only a LOWER close signs negative"
            );
        }
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
        // (see `shadow_persistence::DEDUP_KEY_CANDLES` -- not restated here,
        // because the copy at the top of this file drifted for months) rely
        // on this.
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
        let s = mk_seal(13, 0, TfIndex::M15, 1_716_001_500, 200.75);
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
        let seal = BufferedSeal::new(
            99,
            EXCHANGE_SEGMENT_NSE_FNO,
            TfIndex::M15,
            state,
            Feed::Dhan,
        );
        let row = ShadowSealRow::from_buffered_seal(&seal);
        assert_eq!(row.table_name, "candles_15m");
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
