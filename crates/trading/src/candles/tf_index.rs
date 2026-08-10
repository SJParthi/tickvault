//! Runtime-indexable timeframe handle for the live candle engine.
//!
//! The hot-path multi-TF aggregator needs an O(1) ordinal-indexable
//! mapping from `(timeframe → QuestDB table name + DEDUP key +
//! bucket-seconds + display name)`. [`TfIndex`] is that handle: a
//! `#[repr(u8)]` enum whose ordinal indexes the per-instrument
//! `[Mutex<LiveCandleState>; TF_COUNT]` slot array AND the
//! storage-side `[Sender; TF_COUNT]` ILP writer array.
//!
//! ## The 21 timeframes (1s–15s, 30s, 1m, 3m, 5m, 15m, 1d)
//!
//! The candle re-architecture (#T1) ships ONE aggregator that derives
//! all 21 timeframes directly from the live tick stream and flushes
//! each sealed bar straight to its own plain QuestDB table
//! (`candles_1s` … `candles_1d`; legacy frames write-stopped). There
//! are no `_shadow` tables and no materialized-view cascade — every
//! timeframe is a first-class table written at seal time.
//!
//! Variant ordinals are FROZEN (1m…1d at 0..=4; 1s–15s + 30s
//! appended at 5..=20) so the ordinal
//! returned by [`Self::as_ordinal`] is stable. Reordering variants is
//! a SEMVER break — every consumer indexing by ordinal (the
//! per-instrument `[Mutex<LiveCandleState>; TF_COUNT]`, the ILP
//! `[Sender; TF_COUNT]` writer, the audit-table `timeframe` SYMBOL column)
//! breaks silently.

/// Number of timeframes the live candle engine derives. Pinned here so
/// the per-instrument slot array and the storage-side sender array
/// share one source of truth. 21 since C3 (operator frame directive
/// 2026-07-21): 1s..15s + 30s + 1m/3m/5m/15m + broker 1d — the 16
/// second-scale frames are STRUCTURAL ONLY (GDF-feed-gated, zero rows
/// until the GDF 1s live feed lands in its own lane).
pub const TF_COUNT: usize = 21;

/// 09:15:00 IST expressed as seconds-of-day (`9*3600 + 15*60`).
/// The NSE regular trading session opens at 09:15:00 — every candle
/// bucket grid is anchored here so the first candle of every timeframe
/// starts exactly at the market open.
pub(crate) const MARKET_OPEN_SECS_OF_DAY_IST: u32 = 33_300;

/// 15:30:00 IST expressed as seconds-of-day (`15*3600 + 30*60`).
/// The NSE regular session closes at 15:30:00 — the candle window is
/// the half-open interval `[09:15:00, 15:30:00)`, so the last 1-minute
/// candle is `[15:29:00, 15:30:00)` (stamped 15:29). 375 1m candles/day.
/// Production consumer restored 2026-08-09 with the tick aggregator rebuild:
/// `MultiTfAggregator::consume_tick` gates the candle window on
/// `[MARKET_OPEN_SECS_OF_DAY_IST, MARKET_CLOSE_SECS_OF_DAY_IST)`. (It was
/// `#[cfg(test)]` between the 2026-07-17 stage-3 sweep, which deleted its
/// only caller, and that rebuild.) NOTE the value is 56_400 = 15:40:00 IST,
/// not 15:30 — the NSE CAS change of 2026-08-03; the pin test below asserts
/// it against the common-crate G1 gate so the two can never drift.
pub(crate) const MARKET_CLOSE_SECS_OF_DAY_IST: u32 = 56_400;

/// Runtime-indexable handle for the 21 candle timeframes.
///
/// Use [`Self::ALL`] to iterate. Use [`Self::from_ordinal`] for
/// runtime decoding (e.g. parsing audit-table rows). Use
/// [`Self::table_name`] / [`Self::dedup_key`] /
/// [`Self::seconds_per_bucket`] / [`Self::display_name`] for direct
/// look-up without recomputing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TfIndex {
    /// 1-minute candles (60 s).
    M1 = 0,
    /// 3-minute candles (180 s).
    M3 = 1,
    /// 5-minute candles (300 s).
    M5 = 2,
    /// 15-minute candles (900 s).
    M15 = 3,
    /// 1-day candles (86_400 s — UTC-aligned arithmetic; the
    /// IST-midnight rollover task force-seals open bars at IST 00:00
    /// every trading day so the UTC boundary does not produce stale
    /// candles in practice).
    D1 = 4,
    // -- Second-scale frames (C3, operator directive 2026-07-21) ------
    // APPENDED after D1 so every pre-existing seal-spill ordinal
    // (0..=4) stays byte-stable (SEAL_SPILL_FORMAT_VERSION stays 1).
    // STRUCTURAL ONLY: all 16 frames are GDF-feed-gated — ZERO rows
    // until the GDF 1s live feed lands (separate lane); the REST 1m
    // cadence fold never writes them.
    /// 1-second candles (1 s). GDF-feed-gated (structural).
    S1 = 5,
    /// 2-second candles (2 s). GDF-feed-gated (structural).
    S2 = 6,
    /// 3-second candles (3 s). GDF-feed-gated (structural).
    S3 = 7,
    /// 4-second candles (4 s). GDF-feed-gated (structural).
    S4 = 8,
    /// 5-second candles (5 s). GDF-feed-gated (structural).
    S5 = 9,
    /// 6-second candles (6 s). GDF-feed-gated (structural).
    S6 = 10,
    /// 7-second candles (7 s). GDF-feed-gated (structural).
    S7 = 11,
    /// 8-second candles (8 s). GDF-feed-gated (structural).
    S8 = 12,
    /// 9-second candles (9 s). GDF-feed-gated (structural).
    S9 = 13,
    /// 10-second candles (10 s). GDF-feed-gated (structural).
    S10 = 14,
    /// 11-second candles (11 s). GDF-feed-gated (structural).
    S11 = 15,
    /// 12-second candles (12 s). GDF-feed-gated (structural).
    S12 = 16,
    /// 13-second candles (13 s). GDF-feed-gated (structural).
    S13 = 17,
    /// 14-second candles (14 s). GDF-feed-gated (structural).
    S14 = 18,
    /// 15-second candles (15 s). GDF-feed-gated (structural).
    S15 = 19,
    /// 30-second candles (30 s). GDF-feed-gated (structural).
    S30 = 20,
}

impl TfIndex {
    /// All 21 timeframes in ORDINAL (seal-spill append) order: the 5
    /// legacy frames (1m/3m/5m/15m/1d) first, then the 16 GDF-gated
    /// second-scale frames (1s..15s, 30s) appended by C3 — so the
    /// array is deliberately NOT globally seconds-ascending. The index
    /// of each entry equals its [`Self::as_ordinal`] value, which the
    /// hot-path `[Mutex<LiveCandleState>; TF_COUNT]` array indexing
    /// relies on.
    pub const ALL: [TfIndex; TF_COUNT] = [
        TfIndex::M1,
        TfIndex::M3,
        TfIndex::M5,
        TfIndex::M15,
        TfIndex::D1,
        TfIndex::S1,
        TfIndex::S2,
        TfIndex::S3,
        TfIndex::S4,
        TfIndex::S5,
        TfIndex::S6,
        TfIndex::S7,
        TfIndex::S8,
        TfIndex::S9,
        TfIndex::S10,
        TfIndex::S11,
        TfIndex::S12,
        TfIndex::S13,
        TfIndex::S14,
        TfIndex::S15,
        TfIndex::S30,
    ];

    /// Returns the ordinal (`0..TF_COUNT`) used to index the
    /// per-instrument `[Mutex<LiveCandleState>; TF_COUNT]` array AND the
    /// storage-side `[Sender; TF_COUNT]` ILP writer array.
    #[inline]
    #[must_use]
    pub const fn as_ordinal(self) -> usize {
        self as usize
    }

    /// Decodes an ordinal back to a [`TfIndex`]. Returns `None` for
    /// out-of-range input (`>= TF_COUNT`). Used by the audit-table
    /// reader and any MCP `questdb_sql` consumer that surfaces
    /// `timeframe` SYMBOL rows.
    #[inline]
    #[must_use]
    pub const fn from_ordinal(ord: usize) -> Option<Self> {
        match ord {
            0 => Some(Self::M1),
            1 => Some(Self::M3),
            2 => Some(Self::M5),
            3 => Some(Self::M15),
            4 => Some(Self::D1),
            5 => Some(Self::S1),
            6 => Some(Self::S2),
            7 => Some(Self::S3),
            8 => Some(Self::S4),
            9 => Some(Self::S5),
            10 => Some(Self::S6),
            11 => Some(Self::S7),
            12 => Some(Self::S8),
            13 => Some(Self::S9),
            14 => Some(Self::S10),
            15 => Some(Self::S11),
            16 => Some(Self::S12),
            17 => Some(Self::S13),
            18 => Some(Self::S14),
            19 => Some(Self::S15),
            20 => Some(Self::S30),
            _ => None,
        }
    }

    /// Returns the plain QuestDB table name for this timeframe
    /// (`candles_1m` … `candles_1d`). The seal-time ILP writer uses
    /// this for `Buffer::table(...)`.
    #[inline]
    #[must_use]
    pub const fn table_name(self) -> &'static str {
        match self {
            Self::M1 => "candles_1m",
            Self::M3 => "candles_3m",
            Self::M5 => "candles_5m",
            Self::M15 => "candles_15m",
            Self::D1 => "candles_1d",
            Self::S1 => "candles_1s",
            Self::S2 => "candles_2s",
            Self::S3 => "candles_3s",
            Self::S4 => "candles_4s",
            Self::S5 => "candles_5s",
            Self::S6 => "candles_6s",
            Self::S7 => "candles_7s",
            Self::S8 => "candles_8s",
            Self::S9 => "candles_9s",
            Self::S10 => "candles_10s",
            Self::S11 => "candles_11s",
            Self::S12 => "candles_12s",
            Self::S13 => "candles_13s",
            Self::S14 => "candles_14s",
            Self::S15 => "candles_15s",
            Self::S30 => "candles_30s",
        }
    }

    /// Returns the `DEDUP UPSERT KEYS(...)` column list for this
    /// timeframe's table. Includes the designated timestamp `ts`
    /// explicitly — QuestDB rejects a DEDUP key that omits the
    /// designated timestamp column. The composite `(security_id,
    /// segment)` satisfies the I-P1-11 segment-aware uniqueness rule.
    ///
    /// `feed` (operator lock 2026-06-19, "same tables + feed column") is
    /// part of the key so a Dhan candle and a Groww candle for the SAME
    /// `(ts, security_id, segment)` minute are BOTH kept — distinct broker
    /// feeds are distinct observations, never a duplicate. The Dhan candle
    /// writer stamps a constant `feed='dhan'` and the Groww writer a
    /// constant `feed='groww'`, so the label is replay-stable and does NOT
    /// break the minute-bucket idempotency guarantee. Must equal
    /// `DEDUP_KEY_CANDLES` in `shadow_persistence.rs` (pinned by
    /// `test_dedup_key_candles_matches_tf_index_dedup_key`).
    #[inline]
    #[must_use]
    pub const fn dedup_key(self) -> &'static str {
        "ts, security_id, segment, feed"
    }

    /// Bucket size in seconds.
    #[inline]
    #[must_use]
    pub const fn seconds_per_bucket(self) -> u32 {
        match self {
            Self::M1 => 60,
            Self::M3 => 180,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::D1 => 86_400,
            Self::S1 => 1,
            Self::S2 => 2,
            Self::S3 => 3,
            Self::S4 => 4,
            Self::S5 => 5,
            Self::S6 => 6,
            Self::S7 => 7,
            Self::S8 => 8,
            Self::S9 => 9,
            Self::S10 => 10,
            Self::S11 => 11,
            Self::S12 => 12,
            Self::S13 => 13,
            Self::S14 => 14,
            Self::S15 => 15,
            Self::S30 => 30,
        }
    }

    /// True for the 16 GDF-gated second-scale frames (bucket < 60 s:
    /// 1s..=15s + 30s). These frames are STRUCTURAL until the GDF 1s live
    /// feed lands (separate lane) — ZERO rows are written today, the REST
    /// 1m cadence folds only the 5-frame minute/day set, and the RAM store
    /// allocates them as capacity-1 placeholders (never full session rings).
    #[inline]
    #[must_use]
    pub const fn is_second_scale(self) -> bool {
        self.seconds_per_bucket() < 60
    }

    /// Short display name (`"1m"`, `"3m"`, ..., `"1d"`). Stable across
    /// the codebase and the audit-table `timeframe` SYMBOL column.
    #[inline]
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M3 => "3m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::D1 => "1d",
            Self::S1 => "1s",
            Self::S2 => "2s",
            Self::S3 => "3s",
            Self::S4 => "4s",
            Self::S5 => "5s",
            Self::S6 => "6s",
            Self::S7 => "7s",
            Self::S8 => "8s",
            Self::S9 => "9s",
            Self::S10 => "10s",
            Self::S11 => "11s",
            Self::S12 => "12s",
            Self::S13 => "13s",
            Self::S14 => "14s",
            Self::S15 => "15s",
            Self::S30 => "30s",
        }
    }

    /// Aligns a tick's IST-second timestamp to the start of its
    /// containing bucket for this timeframe.
    ///
    /// Buckets are anchored to the **09:15:00 IST market open**, NOT to
    /// the epoch — so every timeframe's first candle of the day starts
    /// exactly at 09:15 (a 15m bucket is `[09:15,09:30)`; the first
    /// bucket of every frame starts at the open). A tick at or
    /// before the open anchors to the first bucket; the aggregator's
    /// market-hours gate keeps genuine pre-open ticks out anyway.
    ///
    /// `tick_ist_secs` MUST be the IST epoch second derived from the
    /// WS LTT field (NEVER `Utc::now()` per `data-integrity.md`).
    ///
    /// # Saturating by design
    ///
    /// Every add here is `saturating_add`, NOT a bare `+`. The release profile
    /// sets `overflow-checks = true` with `panic = "abort"`, so an arithmetic
    /// overflow does not return a wrong number — it kills the process, and in
    /// the 16-connection feed that means all sixteen sockets, not one tick.
    ///
    /// Today's only production caller (`MultiTfAggregator::consume_tick`)
    /// gates on the session window first, and for any `ts` passing that gate
    /// `ts % 86_400 >= MARKET_OPEN_SECS_OF_DAY_IST`, which algebraically rules
    /// the overflow out. But that is an UNDOCUMENTED PRECONDITION on a `pub
    /// fn`, enforced by a caller rather than by the type system — a second
    /// call site would silently reintroduce a remote abort, since
    /// `tick_ist_secs` is a raw `u32` off the wire that no parser
    /// range-validates. Flagged by two independent adversarial reviews
    /// (2026-08-09). Saturating makes the function safe for ANY input, so the
    /// precondition stops being load-bearing.
    #[inline]
    #[must_use]
    pub const fn bucket_start(self, tick_ist_secs: u32) -> u32 {
        let secs = self.seconds_per_bucket();
        let day_start = (tick_ist_secs / 86_400) * 86_400;
        let market_open = day_start.saturating_add(MARKET_OPEN_SECS_OF_DAY_IST);
        if tick_ist_secs <= market_open {
            return market_open;
        }
        market_open.saturating_add(((tick_ist_secs - market_open) / secs) * secs)
    }

    /// Returns the (exclusive) bucket-end for a given bucket-start.
    /// Equivalent to `bucket_start + seconds_per_bucket()`, saturating.
    ///
    /// Saturating for the same reason as [`Self::bucket_start`]: this one has
    /// NO production caller today, so it has no gate protecting it at all —
    /// it is one direct call away from an abort on a wire-derived value.
    #[inline]
    #[must_use]
    pub const fn bucket_end(self, bucket_start: u32) -> u32 {
        bucket_start.saturating_add(self.seconds_per_bucket())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Session-constant drift pin (operator directive 2026-07-03): the
    /// trading-crate seconds-of-day session constants that gate the candle
    /// grid MUST stay 09:15:00 / 15:30:00 IST AND agree exactly with the
    /// canonical common-crate G1 gate constants (`MARKET_OPEN_IST_NANOS` /
    /// `MARKET_CLOSE_IST_NANOS`, nanos-of-day). If either representation is
    /// edited alone, this test fails the build — the day-OHLC gate
    /// (`day_ohlc_session_accepts` in the app crate) delegates to the
    /// common-crate gate, so this pin keeps ALL session windows identical.
    #[test]
    fn test_session_constants_pinned_and_agree_with_common_crate() {
        use tickvault_common::constants::{MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS};

        assert_eq!(MARKET_OPEN_SECS_OF_DAY_IST, 33_300, "09:15:00 IST");
        assert_eq!(
            MARKET_CLOSE_SECS_OF_DAY_IST, 56_400,
            "15:40:00 IST (exclusive) — NSE CAS change 2026-08-03"
        );
        assert_eq!(
            i64::from(MARKET_OPEN_SECS_OF_DAY_IST) * 1_000_000_000,
            MARKET_OPEN_IST_NANOS,
            "trading-crate open constant drifted from common-crate G1 gate open"
        );
        assert_eq!(
            i64::from(MARKET_CLOSE_SECS_OF_DAY_IST) * 1_000_000_000,
            MARKET_CLOSE_IST_NANOS,
            "trading-crate close constant drifted from common-crate G1 gate close"
        );
    }

    #[test]
    fn test_tf_index_all_has_twenty_one_distinct_variants() {
        let mut seen = std::collections::HashSet::new();
        for tf in TfIndex::ALL {
            assert!(seen.insert(tf), "duplicate variant in TfIndex::ALL: {tf:?}");
        }
        assert_eq!(TfIndex::ALL.len(), TF_COUNT);
        assert_eq!(TF_COUNT, 21);
    }

    /// C3 (2026-07-21): the 16 second-scale frames are APPENDED after
    /// D1 so every pre-existing seal-spill ordinal (0..=4) stays
    /// stable — `ALL` is therefore ordinal-ordered, NOT globally
    /// seconds-ascending. This pin is strictly stronger than the old
    /// ascending check: it pins the EXACT seconds sequence, and each
    /// ordinal block stays strictly ascending within itself.
    #[test]
    fn test_tf_index_ordinal_order_pins_exact_seconds_sequence() {
        let secs: Vec<u32> = TfIndex::ALL
            .iter()
            .map(|tf| tf.seconds_per_bucket())
            .collect();
        let expected: Vec<u32> = [60_u32, 180, 300, 900, 86_400]
            .into_iter()
            .chain(1..=15)
            .chain([30])
            .collect();
        assert_eq!(secs, expected, "ordinal seconds sequence drifted");
        for block in [&secs[..5], &secs[5..]] {
            for window in block.windows(2) {
                assert!(
                    window[0] < window[1],
                    "each ordinal block must be strictly ascending by \
                     seconds_per_bucket; got {} >= {}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    /// APPEND-ONLY ORDINAL PIN (2026-08-09, added with the tick-aggregator
    /// rebuild).
    ///
    /// Every other ordinal test in this file derives its expectation from
    /// `TfIndex::ALL` or from `as u8`, so all of them stay green if a variant
    /// is INSERTED in the middle and `ALL` is updated to match. That edit is
    /// exactly the catastrophic one: the ordinal is serialised into
    /// seal-spill records ON DISK (`SEAL_SPILL_FORMAT_VERSION` 1) and used as
    /// the `[…; TF_COUNT]` slot index, so inserting `S45` between `D1` and
    /// `S1` would silently re-interpret every already-spilled ordinal ≥ 5 as
    /// a different timeframe — historical bars re-mapped, no error anywhere.
    ///
    /// This test pins each variant to a LITERAL integer. Appending a new
    /// variant at the end leaves every literal below untouched and passes;
    /// inserting or reordering fails the build. Add new frames HERE at the
    /// bottom only.
    #[test]
    /// ADVERSARIAL REGRESSION (2026-08-09, flagged by two independent
    /// reviews). `tick_ist_secs` is a raw `u32` off the wire that no parser
    /// range-validates. The release profile is `overflow-checks = true` with
    /// `panic = "abort"`, so an overflow here does not produce a wrong candle
    /// — it aborts the process, taking all sixteen sockets down. These must
    /// return a saturated value for EVERY input, with no caller-side gate.
    #[test]
    fn test_bucket_math_saturates_instead_of_aborting_on_extreme_inputs() {
        for tf in TfIndex::ALL {
            for ts in [u32::MAX, u32::MAX - 1, u32::MAX - 86_399, 0, 1] {
                let start = tf.bucket_start(ts);
                // Reaching here at all is the assertion: a bare `+` would have
                // aborted the test process under overflow-checks.
                let end = tf.bucket_end(start);
                assert!(
                    end >= start,
                    "{tf:?}: bucket_end({start}) = {end} must never wrap below \
                     its start for ts {ts}"
                );
            }
            // The saturating ceiling is reachable and stable.
            assert_eq!(
                tf.bucket_end(u32::MAX),
                u32::MAX,
                "{tf:?}: bucket_end must clamp at u32::MAX, not wrap to 0"
            );
        }
    }

    #[test]
    fn test_tf_index_ordinals_are_append_only_literal_pins() {
        assert_eq!(TfIndex::M1 as u8, 0);
        assert_eq!(TfIndex::M3 as u8, 1);
        assert_eq!(TfIndex::M5 as u8, 2);
        assert_eq!(TfIndex::M15 as u8, 3);
        assert_eq!(TfIndex::D1 as u8, 4);
        assert_eq!(TfIndex::S1 as u8, 5);
        assert_eq!(TfIndex::S2 as u8, 6);
        assert_eq!(TfIndex::S3 as u8, 7);
        assert_eq!(TfIndex::S4 as u8, 8);
        assert_eq!(TfIndex::S5 as u8, 9);
        assert_eq!(TfIndex::S6 as u8, 10);
        assert_eq!(TfIndex::S7 as u8, 11);
        assert_eq!(TfIndex::S8 as u8, 12);
        assert_eq!(TfIndex::S9 as u8, 13);
        assert_eq!(TfIndex::S10 as u8, 14);
        assert_eq!(TfIndex::S11 as u8, 15);
        assert_eq!(TfIndex::S12 as u8, 16);
        assert_eq!(TfIndex::S13 as u8, 17);
        assert_eq!(TfIndex::S14 as u8, 18);
        assert_eq!(TfIndex::S15 as u8, 19);
        assert_eq!(TfIndex::S30 as u8, 20);
        // The pinned block above must cover EVERY variant: a new appended
        // frame that nobody pinned would slip through otherwise.
        assert_eq!(
            TF_COUNT, 21,
            "a frame was added — pin its literal ordinal above"
        );
        // …and the seconds are pinned per-ordinal too, so a variant cannot be
        // re-pointed at a different bucket size while keeping its ordinal.
        assert_eq!(TfIndex::M1.seconds_per_bucket(), 60);
        assert_eq!(TfIndex::D1.seconds_per_bucket(), 86_400);
        assert_eq!(TfIndex::S1.seconds_per_bucket(), 1);
        assert_eq!(TfIndex::S30.seconds_per_bucket(), 30);
    }
    #[test]
    fn test_tf_index_ordinal_round_trip() {
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            assert_eq!(tf.as_ordinal(), idx, "ordinal mismatch for {tf:?}");
            assert_eq!(
                TfIndex::from_ordinal(idx),
                Some(*tf),
                "from_ordinal({idx}) failed to roundtrip"
            );
        }
    }

    #[test]
    fn test_tf_index_from_ordinal_rejects_out_of_range() {
        assert_eq!(TfIndex::from_ordinal(TF_COUNT), None);
        assert_eq!(TfIndex::from_ordinal(usize::MAX), None);
    }

    #[test]
    fn test_tf_index_table_names_are_plain_and_canonical() {
        let names: [&str; TF_COUNT] = std::array::from_fn(|i| {
            TfIndex::from_ordinal(i)
                .expect("ordinal in range")
                .table_name()
        });
        let expected = [
            "candles_1m",
            "candles_3m",
            "candles_5m",
            "candles_15m",
            "candles_1d",
            "candles_1s",
            "candles_2s",
            "candles_3s",
            "candles_4s",
            "candles_5s",
            "candles_6s",
            "candles_7s",
            "candles_8s",
            "candles_9s",
            "candles_10s",
            "candles_11s",
            "candles_12s",
            "candles_13s",
            "candles_14s",
            "candles_15s",
            "candles_30s",
        ];
        assert_eq!(names, expected);
        // No `_shadow` suffix anywhere — these are first-class tables.
        for name in names {
            assert!(!name.contains("shadow"), "{name} must be a plain table");
        }
    }

    #[test]
    fn test_tf_index_table_names_unique() {
        let mut seen = std::collections::HashSet::new();
        for tf in TfIndex::ALL {
            assert!(
                seen.insert(tf.table_name()),
                "duplicate table_name {}",
                tf.table_name()
            );
        }
    }

    #[test]
    fn test_tf_index_dedup_key_includes_ts_and_segment_for_i_p1_11() {
        // QuestDB rejects a DEDUP key that omits the designated
        // timestamp; I-P1-11 requires the segment alongside security_id.
        for tf in TfIndex::ALL {
            let key = tf.dedup_key();
            assert!(
                key.contains("ts"),
                "{} dedup key missing ts",
                tf.display_name()
            );
            assert!(
                key.contains("security_id"),
                "{} dedup key missing security_id",
                tf.display_name()
            );
            assert!(
                key.contains("segment"),
                "I-P1-11 violation: {} dedup key {:?} missing segment",
                tf.display_name(),
                key
            );
            assert!(
                key.contains("feed"),
                "feed-in-key (operator 2026-06-19): {} dedup key {:?} missing feed",
                tf.display_name(),
                key
            );
        }
    }

    #[test]
    fn test_tf_index_display_names_unique_and_stable() {
        let mut seen = std::collections::HashSet::new();
        let expected = [
            "1m", "3m", "5m", "15m", "1d", "1s", "2s", "3s", "4s", "5s", "6s", "7s", "8s", "9s",
            "10s", "11s", "12s", "13s", "14s", "15s", "30s",
        ];
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            let name = tf.display_name();
            assert_eq!(
                name, expected[idx],
                "display_name diverged at ordinal {idx}"
            );
            assert!(seen.insert(name), "duplicate display_name {name}");
        }
    }

    #[test]
    fn test_tf_index_seconds_per_bucket_values() {
        assert_eq!(TfIndex::M1.seconds_per_bucket(), 60);
        assert_eq!(TfIndex::M3.seconds_per_bucket(), 180);
        assert_eq!(TfIndex::M5.seconds_per_bucket(), 300);
        assert_eq!(TfIndex::M15.seconds_per_bucket(), 900);
        assert_eq!(TfIndex::D1.seconds_per_bucket(), 86_400);
        // Second-scale frames (C3): S1..S15 are 1..=15 s, S30 is 30 s.
        for ord in 5..=19_usize {
            let tf = TfIndex::from_ordinal(ord).expect("second-scale ordinal");
            assert_eq!(
                tf.seconds_per_bucket(),
                u32::try_from(ord - 4).expect("fits"),
                "S-frame seconds drifted at ordinal {ord}"
            );
        }
        assert_eq!(TfIndex::S30.seconds_per_bucket(), 30);
        // Every minute-class TF is a whole number of minutes.
        for tf in [TfIndex::M1, TfIndex::M3, TfIndex::M5, TfIndex::M15] {
            assert_eq!(tf.seconds_per_bucket() % 60, 0);
        }
    }

    #[test]
    fn test_tf_index_second_scale_gate_is_exactly_the_16_gdf_frames() {
        // GDF-gate predicate: exactly the 16 sub-minute frames (S1..S15 +
        // S30) are second-scale; the legacy 5-frame live set is NOT.
        let second_scale: Vec<TfIndex> = TfIndex::ALL
            .into_iter()
            .filter(|tf| tf.is_second_scale())
            .collect();
        assert_eq!(second_scale.len(), 16, "second-scale frame count drifted");
        for tf in [
            TfIndex::M1,
            TfIndex::M3,
            TfIndex::M5,
            TfIndex::M15,
            TfIndex::D1,
        ] {
            assert!(!tf.is_second_scale(), "{tf:?} wrongly GDF-gated");
        }
        for tf in second_scale {
            assert!(
                tf.seconds_per_bucket() < 60,
                "{tf:?} gated but not sub-minute"
            );
            assert!(
                tf.as_ordinal() >= 5,
                "{tf:?} gated frame in legacy ordinals"
            );
        }
    }

    #[test]
    fn test_tf_index_bucket_start_aligns_to_seconds_per_bucket() {
        // An in-window IST tick (~11:24 IST). Buckets anchor to the
        // 09:15:00 market open, NOT the epoch.
        let tick = 1_779_362_677_u32;
        let market_open = (tick / 86_400) * 86_400 + 33_300;
        for tf in TfIndex::ALL {
            let bucket = tf.bucket_start(tick);
            let secs = tf.seconds_per_bucket();
            assert!(
                bucket <= tick,
                "bucket_start past input for {}",
                tf.display_name()
            );
            assert_eq!(
                (bucket - market_open) % secs,
                0,
                "bucket_start not anchored to 09:15 for {}",
                tf.display_name()
            );
            assert!(
                tick - bucket < secs,
                "bucket_start too far below input for {}",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_tf_index_bucket_start_idempotent_on_aligned_input() {
        let tick = 1_779_362_677_u32;
        for tf in TfIndex::ALL {
            let bucket = tf.bucket_start(tick);
            assert_eq!(
                tf.bucket_start(bucket),
                bucket,
                "bucket_start should be idempotent on a bucket boundary for {}",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_tf_index_bucket_end_equals_start_plus_secs() {
        for tf in TfIndex::ALL {
            let start = tf.bucket_start(1_716_192_000_u32);
            let end = tf.bucket_end(start);
            assert_eq!(end - start, tf.seconds_per_bucket());
        }
    }

    #[test]
    fn test_tf_index_repr_u8_matches_ordinal() {
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            assert_eq!(*tf as u8, u8::try_from(idx).expect("ordinal fits u8"));
        }
    }

    /// `Ord` sorts by the `repr(u8)` discriminant = the seal-spill
    /// APPEND order (C3) — which is exactly `ALL`'s order. Deliberately
    /// NOT seconds order: D1 (ordinal 4) precedes S1 (ordinal 5).
    #[test]
    fn test_tf_index_total_ordering_matches_ordinal_append_order() {
        let mut sorted = TfIndex::ALL.to_vec();
        sorted.sort();
        assert_eq!(sorted, TfIndex::ALL);
    }
}
