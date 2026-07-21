//! Runtime-indexable timeframe handle for the live candle engine.
//!
//! The hot-path multi-TF aggregator needs an O(1) ordinal-indexable
//! mapping from `(timeframe → QuestDB table name + DEDUP key +
//! bucket-seconds + display name)`. [`TfIndex`] is that handle: a
//! `#[repr(u8)]` enum whose ordinal indexes the per-instrument
//! `[Mutex<LiveCandleState>; 5]` slot array AND the storage-side
//! `[Sender; 5]` ILP writer array.
//!
//! ## The 5 timeframes
//!
//! The candle re-architecture (#T1) ships ONE aggregator that derives
//! all 5 timeframes directly from the live tick stream and flushes
//! each sealed bar straight to its own plain QuestDB table
//! (`candles_1m` … `candles_1d`). There are no `_shadow` tables, no
//! `candles_1s` base table, and no materialized-view cascade — every
//! timeframe is a first-class table written at seal time.
//!
//! Variants are ordered short-to-long (1m → 1d) so the ordinal
//! returned by [`Self::as_ordinal`] is stable. Reordering variants is
//! a SEMVER break — every consumer indexing by ordinal (the
//! per-instrument `[Mutex<LiveCandleState>; 5]`, the ILP
//! `[Sender; 5]` writer, the audit-table `timeframe` SYMBOL column)
//! breaks silently.

/// Number of timeframes the live candle engine derives. Pinned here so
/// the per-instrument slot array and the storage-side sender array
/// share one source of truth.
pub const TF_COUNT: usize = 5;

/// 09:15:00 IST expressed as seconds-of-day (`9*3600 + 15*60`).
/// The NSE regular trading session opens at 09:15:00 — every candle
/// bucket grid is anchored here so the first candle of every timeframe
/// starts exactly at the market open.
pub(crate) const MARKET_OPEN_SECS_OF_DAY_IST: u32 = 33_300;

/// 15:30:00 IST expressed as seconds-of-day (`15*3600 + 30*60`).
/// The NSE regular session closes at 15:30:00 — the candle window is
/// the half-open interval `[09:15:00, 15:30:00)`, so the last 1-minute
/// candle is `[15:29:00, 15:30:00)` (stamped 15:29). 375 1m candles/day.
/// Test-only since the stage-3 dead-WS sweep (2026-07-17): its sole
/// production consumer was the DELETED tick aggregator's session-window
/// truncation; the pin tests below keep the canonical value asserted.
#[cfg(test)]
pub(crate) const MARKET_CLOSE_SECS_OF_DAY_IST: u32 = 55_800;

/// Runtime-indexable handle for the 5 candle timeframes.
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
}

impl TfIndex {
    /// All 5 timeframes in canonical short-to-long order. The index
    /// of each entry equals its [`Self::as_ordinal`] value, which the
    /// hot-path `[Mutex<LiveCandleState>; 5]` array indexing relies on.
    pub const ALL: [TfIndex; TF_COUNT] = [
        TfIndex::M1,
        TfIndex::M3,
        TfIndex::M5,
        TfIndex::M15,
        TfIndex::D1,
    ];

    /// Returns the ordinal (`0..TF_COUNT`) used to index the
    /// per-instrument `[Mutex<LiveCandleState>; 5]` array AND the
    /// storage-side `[Sender; 5]` ILP writer array.
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
        }
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
    #[inline]
    #[must_use]
    pub const fn bucket_start(self, tick_ist_secs: u32) -> u32 {
        let secs = self.seconds_per_bucket();
        let day_start = (tick_ist_secs / 86_400) * 86_400;
        let market_open = day_start + MARKET_OPEN_SECS_OF_DAY_IST;
        if tick_ist_secs <= market_open {
            return market_open;
        }
        market_open + ((tick_ist_secs - market_open) / secs) * secs
    }

    /// Returns the (exclusive) bucket-end for a given bucket-start.
    /// Equivalent to `bucket_start + seconds_per_bucket()`.
    #[inline]
    #[must_use]
    pub const fn bucket_end(self, bucket_start: u32) -> u32 {
        bucket_start + self.seconds_per_bucket()
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
            MARKET_CLOSE_SECS_OF_DAY_IST, 55_800,
            "15:30:00 IST (exclusive)"
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
    fn test_tf_index_all_has_five_distinct_variants() {
        let mut seen = std::collections::HashSet::new();
        for tf in TfIndex::ALL {
            assert!(seen.insert(tf), "duplicate variant in TfIndex::ALL: {tf:?}");
        }
        assert_eq!(TfIndex::ALL.len(), TF_COUNT);
        assert_eq!(TF_COUNT, 5);
    }

    #[test]
    fn test_tf_index_canonical_ordering_short_to_long() {
        let secs: Vec<u32> = TfIndex::ALL
            .iter()
            .map(|tf| tf.seconds_per_bucket())
            .collect();
        for window in secs.windows(2) {
            assert!(
                window[0] < window[1],
                "TfIndex::ALL must be strictly ascending by seconds_per_bucket; \
                 got {} >= {}",
                window[0],
                window[1]
            );
        }
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
        let expected = ["1m", "3m", "5m", "15m", "1d"];
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
        // Every minute-class TF is a whole number of minutes.
        for tf in [TfIndex::M1, TfIndex::M3, TfIndex::M5, TfIndex::M15] {
            assert_eq!(tf.seconds_per_bucket() % 60, 0);
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

    #[test]
    fn test_tf_index_canonical_total_ordering_matches_secs_ordering() {
        let mut sorted = TfIndex::ALL.to_vec();
        sorted.sort();
        assert_eq!(sorted, TfIndex::ALL);
    }
}
