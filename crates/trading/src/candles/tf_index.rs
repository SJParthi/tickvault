//! Wave 6 Sub-PR #1 — bridge between the existing `Timeframe` markers
//! (`engine.rs::{Tf1m, Tf5m, ..., Tf1d}`) and the 9 shadow tables
//! defined in PR #551 (`tickvault_storage::shadow_persistence`).
//!
//! This module exists because the Wave 6 hot-path multi-TF aggregator
//! needs an O(1) ordinal-indexable mapping from `(timeframe → shadow
//! table name + DEDUP key + bucket-seconds + display name)`. The
//! existing `Timeframe` trait carries `SECS` and `NAME` but is a
//! type-level handle; the new direct-flush writer needs a runtime
//! enum so a single `[Sender; 9]` ILP sender array can be indexed by
//! `TfIndex::as_ordinal()`.
//!
//! Per the locked design decisions in
//! `.claude/plans/active-plan-aggregator-direct-flush-rehydrate.md`
//! Sub-PR #1:
//!
//! - **L-C2** — `[AtomicCell<LiveCandleState>; 9]` indexed by `TfIndex`.
//! - **L-H10** — 9 shadow tables `candles_{tf}_shadow` with DEDUP UPSERT
//!   KEYS `(ts, security_id, exchange_segment)`.
//! - **L-L22** — canonical 1m → 1d ordering (matches the array index
//!   order returned by `tickvault_storage::shadow_persistence::shadow_candle_table_names`).
//!
//! ## What this module does NOT yet contain
//!
//! - The `[AtomicCell<LiveCandleState>; 9]` cell type itself
//!   (cell type lands in the next commit on this branch, after
//!   operator approval to add `crossbeam-utils` or `parking_lot` to
//!   workspace deps; the foundation `LiveCandleState` struct shape +
//!   bucket-alignment helpers ride along too).
//! - The `ShadowBarWriter` ring → spill → DLQ writer
//!   (Sub-PR #1 item 1.2, follow-up commit).
//! - The boundary timer
//!   (Sub-PR #1 item 1.3, follow-up commit).
//!
//! Cross-refs:
//! - `crates/storage/src/shadow_persistence.rs` (PR #551 — shadow tables)
//! - `crates/trading/src/candles/engine.rs` — the `Timeframe` trait
//!   and the 9 marker types this module bridges to.
//! - `.claude/rules/project/wave-6-error-codes.md` — Wave 6 ErrorCode
//!   runbook (used by the writer commit).

use crate::candles::{Tf1d, Tf1h, Tf1m, Tf2h, Tf3h, Tf4h, Tf5m, Tf15m, Tf30m, Timeframe};

/// Runtime-indexable handle for the 9 timeframes shipped to shadow
/// tables in Wave 6 Sub-PR #1.
///
/// Variants are ordered short-to-long (1m → 1d) so the ordinal
/// returned by [`Self::as_ordinal`] is stable and matches the canonical
/// order returned by
/// `tickvault_storage::shadow_persistence::shadow_candle_table_names`.
/// Reordering variants is a SEMVER break — every consumer indexing by
/// ordinal (the forthcoming `[AtomicCell<LiveCandleState>; 9]` per
/// instrument, the ILP `[Sender; 9]` writer, the audit-table
/// `timeframe` SYMBOL column) breaks silently.
///
/// The 9 timeframes match the Wave-5 retained ladder
/// (`engine.rs:172-180` comment): the 12 sub-15-minute non-canonical
/// timeframes were retired in PR #517. Wave 6 deliberately ships ONLY
/// the 9 retained timeframes — sub-minute and weekly/monthly
/// timeframes are NOT shadow-table-backed in this plan.
///
/// Use [`Self::ALL`] to iterate. Use [`Self::from_ordinal`] for
/// runtime decoding (e.g. parsing audit-table rows). Use
/// [`Self::shadow_table_name`] / [`Self::shadow_dedup_key`] /
/// [`Self::seconds_per_bucket`] / [`Self::display_name`] for direct
/// look-up without recomputing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TfIndex {
    /// 1-minute candles (60 s).
    M1 = 0,
    /// 5-minute candles (300 s).
    M5 = 1,
    /// 15-minute candles (900 s).
    M15 = 2,
    /// 30-minute candles (1_800 s).
    M30 = 3,
    /// 1-hour candles (3_600 s).
    H1 = 4,
    /// 2-hour candles (7_200 s).
    H2 = 5,
    /// 3-hour candles (10_800 s).
    H3 = 6,
    /// 4-hour candles (14_400 s).
    H4 = 7,
    /// 1-day candles (86_400 s — UTC-aligned per `Tf1d` doc; the
    /// IST-midnight rollover task force-seals open bars at IST 00:00
    /// every trading day so the UTC boundary does not produce stale
    /// candles in practice).
    D1 = 8,
}

impl TfIndex {
    /// All 9 timeframes in canonical short-to-long order. The index
    /// of each entry equals its [`Self::as_ordinal`] value, which the
    /// hot-path `[AtomicCell<LiveCandleState>; 9]` array indexing
    /// relies on.
    pub const ALL: [TfIndex; 9] = [
        TfIndex::M1,
        TfIndex::M5,
        TfIndex::M15,
        TfIndex::M30,
        TfIndex::H1,
        TfIndex::H2,
        TfIndex::H3,
        TfIndex::H4,
        TfIndex::D1,
    ];

    /// Returns the ordinal (0..=8) used to index the per-instrument
    /// `[AtomicCell<LiveCandleState>; 9]` array AND the
    /// `tickvault_storage::shadow_persistence::shadow_candle_table_names`
    /// return value.
    #[inline]
    #[must_use]
    pub const fn as_ordinal(self) -> usize {
        self as usize
    }

    /// Decodes an ordinal back to a [`TfIndex`]. Returns `None` for
    /// out-of-range input (≥ 9). Used by the audit-table reader and
    /// any future MCP `questdb_sql` consumer that surfaces
    /// `timeframe` SYMBOL rows.
    #[inline]
    #[must_use]
    pub const fn from_ordinal(ord: usize) -> Option<Self> {
        match ord {
            0 => Some(Self::M1),
            1 => Some(Self::M5),
            2 => Some(Self::M15),
            3 => Some(Self::M30),
            4 => Some(Self::H1),
            5 => Some(Self::H2),
            6 => Some(Self::H3),
            7 => Some(Self::H4),
            8 => Some(Self::D1),
            _ => None,
        }
    }

    /// Returns the QuestDB shadow table name for this timeframe.
    /// Matches the constants in
    /// `tickvault_storage::shadow_persistence::QUESTDB_TABLE_CANDLES_*M_SHADOW`
    /// exactly so the forthcoming `ShadowBarWriter` can use this for
    /// `Buffer::table(...)` without re-importing storage constants.
    #[inline]
    #[must_use]
    pub const fn shadow_table_name(self) -> &'static str {
        match self {
            Self::M1 => "candles_1m_shadow",
            Self::M5 => "candles_5m_shadow",
            Self::M15 => "candles_15m_shadow",
            Self::M30 => "candles_30m_shadow",
            Self::H1 => "candles_1h_shadow",
            Self::H2 => "candles_2h_shadow",
            Self::H3 => "candles_3h_shadow",
            Self::H4 => "candles_4h_shadow",
            Self::D1 => "candles_1d_shadow",
        }
    }

    /// Returns the DEDUP UPSERT KEY suffix for this timeframe's shadow
    /// table. Matches `tickvault_storage::shadow_persistence::DEDUP_KEY_CANDLES_*M_SHADOW`.
    /// All 9 timeframes share the same suffix `"security_id, exchange_segment"`
    /// per locked decision L-H10 (composite I-P1-11 key); future schema
    /// changes that diverge per-TF would update this method per-variant.
    #[inline]
    #[must_use]
    pub const fn shadow_dedup_key(self) -> &'static str {
        // All 9 share the same key today; encoded per-variant so
        // future divergence is a single-line change without breaking
        // callers.
        match self {
            Self::M1
            | Self::M5
            | Self::M15
            | Self::M30
            | Self::H1
            | Self::H2
            | Self::H3
            | Self::H4
            | Self::D1 => "security_id, exchange_segment",
        }
    }

    /// Bucket size in seconds — the same value carried by the
    /// existing `Timeframe::SECS` associated constant.
    #[inline]
    #[must_use]
    pub const fn seconds_per_bucket(self) -> u32 {
        match self {
            Self::M1 => 60,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::M30 => 1_800,
            Self::H1 => 3_600,
            Self::H2 => 7_200,
            Self::H3 => 10_800,
            Self::H4 => 14_400,
            Self::D1 => 86_400,
        }
    }

    /// Short display name (`"1m"`, `"5m"`, ..., `"1d"`) — same string
    /// the existing `Timeframe::NAME` carries. Stable across the
    /// codebase and the audit-table `timeframe` SYMBOL column.
    #[inline]
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::M30 => "30m",
            Self::H1 => "1h",
            Self::H2 => "2h",
            Self::H3 => "3h",
            Self::H4 => "4h",
            Self::D1 => "1d",
        }
    }

    /// Aligns a tick's IST-second timestamp to the start of its
    /// containing bucket for this timeframe. Matches the existing
    /// `Timeframe::bucket_start` math.
    ///
    /// Used by the boundary timer (Sub-PR #1 item 1.3) and the
    /// missed-boundary catch-up logic (BOUNDARY-01) to compute the
    /// bucket-open `ts` for a sealed candle.
    ///
    /// `tick_ist_secs` MUST be the IST epoch second derived from the
    /// WS LTT field (NEVER `Utc::now()` per locked decision L-H7 and
    /// `data-integrity.md`).
    #[inline]
    #[must_use]
    pub const fn bucket_start(self, tick_ist_secs: u32) -> u32 {
        let secs = self.seconds_per_bucket();
        tick_ist_secs - (tick_ist_secs % secs)
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
// Compile-time agreement with the type-level `Timeframe` trait.
//
// These const assertions fail to compile if `Timeframe::SECS` ever
// drifts from `TfIndex::seconds_per_bucket()` for any of the 9
// shadow-table-backed timeframes. Catches the regression pre-runtime
// without depending on `cargo test`.
// ---------------------------------------------------------------------------

const _: () = assert!(<Tf1m as Timeframe>::SECS == TfIndex::M1.seconds_per_bucket());
const _: () = assert!(<Tf5m as Timeframe>::SECS == TfIndex::M5.seconds_per_bucket());
const _: () = assert!(<Tf15m as Timeframe>::SECS == TfIndex::M15.seconds_per_bucket());
const _: () = assert!(<Tf30m as Timeframe>::SECS == TfIndex::M30.seconds_per_bucket());
const _: () = assert!(<Tf1h as Timeframe>::SECS == TfIndex::H1.seconds_per_bucket());
const _: () = assert!(<Tf2h as Timeframe>::SECS == TfIndex::H2.seconds_per_bucket());
const _: () = assert!(<Tf3h as Timeframe>::SECS == TfIndex::H3.seconds_per_bucket());
const _: () = assert!(<Tf4h as Timeframe>::SECS == TfIndex::H4.seconds_per_bucket());
const _: () = assert!(<Tf1d as Timeframe>::SECS == TfIndex::D1.seconds_per_bucket());

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tf_index_all_has_nine_distinct_variants() {
        let mut seen = std::collections::HashSet::new();
        for tf in TfIndex::ALL {
            assert!(seen.insert(tf), "duplicate variant in TfIndex::ALL: {tf:?}");
        }
        assert_eq!(TfIndex::ALL.len(), 9);
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
        assert_eq!(TfIndex::from_ordinal(9), None);
        assert_eq!(TfIndex::from_ordinal(usize::MAX), None);
    }

    #[test]
    fn test_tf_index_shadow_table_names_match_storage_constants_literally() {
        // The forthcoming hot-path `[Sender; 9]` writer in the storage
        // crate indexes by `TfIndex::as_ordinal()`. To keep the two
        // sides aligned WITHOUT introducing a `tickvault-trading →
        // tickvault-storage` dep cycle (the workspace flow is
        // common ← core ← trading ← storage ← api ← app), we pin
        // the strings literally on BOTH sides. This test verifies
        // the trading-side view; the symmetric verification lives in
        // `crates/storage/src/shadow_persistence.rs::tests::test_canonical_table_name_pattern`
        // (which pins the same 9 strings in canonical order).
        //
        // If a future change diverges one side from the other, BOTH
        // tests fail with the literal string mismatch — operator
        // sees the gap immediately.
        let bridge_names: [&str; 9] = std::array::from_fn(|i| {
            TfIndex::from_ordinal(i)
                .expect("ordinal in 0..9")
                .shadow_table_name()
        });
        let expected = [
            "candles_1m_shadow",
            "candles_5m_shadow",
            "candles_15m_shadow",
            "candles_30m_shadow",
            "candles_1h_shadow",
            "candles_2h_shadow",
            "candles_3h_shadow",
            "candles_4h_shadow",
            "candles_1d_shadow",
        ];
        assert_eq!(bridge_names, expected);
    }

    #[test]
    fn test_tf_index_dedup_key_matches_storage_constant_literally() {
        // Every TfIndex variant returns the I-P1-11 composite key
        // `"security_id, exchange_segment"` per L-H10. Symmetric
        // verification on the storage side lives in
        // `dedup_segment_meta_guard.rs` (workspace-wide scan) +
        // `shadow_persistence.rs::tests::test_every_dedup_key_includes_exchange_segment`.
        for tf in TfIndex::ALL {
            assert_eq!(
                tf.shadow_dedup_key(),
                "security_id, exchange_segment",
                "DEDUP key for {} diverged from canonical I-P1-11 composite",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_tf_index_dedup_keys_all_contain_segment_for_i_p1_11() {
        // Composite key compliance: every shadow table's DEDUP key
        // MUST contain `exchange_segment` per
        // `.claude/rules/project/security-id-uniqueness.md`. The
        // workspace-wide `dedup_segment_meta_guard.rs` enforces this
        // at the storage-crate level; this test pins the bridge
        // module's view of the same invariant.
        for tf in TfIndex::ALL {
            let key = tf.shadow_dedup_key();
            assert!(
                key.contains("exchange_segment"),
                "I-P1-11 violation: {} dedup key {:?} missing `exchange_segment`",
                tf.display_name(),
                key
            );
            assert!(
                key.contains("security_id"),
                "{} dedup key {:?} missing `security_id`",
                tf.display_name(),
                key
            );
        }
    }

    #[test]
    fn test_tf_index_display_names_unique_and_stable() {
        let mut seen = std::collections::HashSet::new();
        let expected = ["1m", "5m", "15m", "30m", "1h", "2h", "3h", "4h", "1d"];
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
    fn test_tf_index_seconds_per_bucket_matches_existing_timeframe_secs() {
        // The compile-time `assert!` blocks above already pin this for
        // each TF, but a runtime test makes the contract visible to
        // grep + IDE jump-to-test workflows.
        assert_eq!(<Tf1m as Timeframe>::SECS, TfIndex::M1.seconds_per_bucket());
        assert_eq!(<Tf5m as Timeframe>::SECS, TfIndex::M5.seconds_per_bucket());
        assert_eq!(
            <Tf15m as Timeframe>::SECS,
            TfIndex::M15.seconds_per_bucket()
        );
        assert_eq!(
            <Tf30m as Timeframe>::SECS,
            TfIndex::M30.seconds_per_bucket()
        );
        assert_eq!(<Tf1h as Timeframe>::SECS, TfIndex::H1.seconds_per_bucket());
        assert_eq!(<Tf2h as Timeframe>::SECS, TfIndex::H2.seconds_per_bucket());
        assert_eq!(<Tf3h as Timeframe>::SECS, TfIndex::H3.seconds_per_bucket());
        assert_eq!(<Tf4h as Timeframe>::SECS, TfIndex::H4.seconds_per_bucket());
        assert_eq!(<Tf1d as Timeframe>::SECS, TfIndex::D1.seconds_per_bucket());
    }

    #[test]
    fn test_tf_index_bucket_start_aligns_to_seconds_per_bucket() {
        // Bucket-start math must be a true downward floor at the
        // bucket-seconds granularity. Tested across all 9 TFs with an
        // arbitrary "now" that's NOT bucket-aligned for any of them.
        let arbitrary = 123_456_789_u32; // not divisible by 60/300/900/1800/3600/...
        for tf in TfIndex::ALL {
            let bucket = tf.bucket_start(arbitrary);
            let secs = tf.seconds_per_bucket();
            assert!(
                bucket <= arbitrary,
                "bucket_start {} > input {} for tf {}",
                bucket,
                arbitrary,
                tf.display_name()
            );
            assert_eq!(
                bucket % secs,
                0,
                "bucket_start {} not aligned to {} for tf {}",
                bucket,
                secs,
                tf.display_name()
            );
            assert!(
                arbitrary - bucket < secs,
                "bucket_start {} too far below input {} for tf {}",
                bucket,
                arbitrary,
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_tf_index_bucket_start_idempotent_on_aligned_input() {
        for tf in TfIndex::ALL {
            let secs = tf.seconds_per_bucket();
            let aligned = secs.saturating_mul(7);
            assert_eq!(
                tf.bucket_start(aligned),
                aligned,
                "bucket_start should be idempotent on already-aligned input for {}",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_tf_index_bucket_end_equals_start_plus_secs() {
        for tf in TfIndex::ALL {
            let start = tf.bucket_start(1_716_192_000_u32); // arbitrary
            let end = tf.bucket_end(start);
            assert_eq!(end - start, tf.seconds_per_bucket());
        }
    }

    #[test]
    fn test_tf_index_repr_u8_matches_ordinal() {
        // `#[repr(u8)]` + explicit discriminants → cast to u8 == as_ordinal.
        // This isn't load-bearing for any call site today but pins the
        // representation so a future change that adds custom Drop or
        // similar can be detected.
        assert_eq!(TfIndex::M1 as u8, 0);
        assert_eq!(TfIndex::M5 as u8, 1);
        assert_eq!(TfIndex::M15 as u8, 2);
        assert_eq!(TfIndex::M30 as u8, 3);
        assert_eq!(TfIndex::H1 as u8, 4);
        assert_eq!(TfIndex::H2 as u8, 5);
        assert_eq!(TfIndex::H3 as u8, 6);
        assert_eq!(TfIndex::H4 as u8, 7);
        assert_eq!(TfIndex::D1 as u8, 8);
    }

    #[test]
    fn test_tf_index_canonical_total_ordering_matches_secs_ordering() {
        // PartialOrd + Ord on TfIndex are derived; the variant order
        // (M1 < M5 < ... < D1) thus matches seconds ascending. This
        // test pins the contract: a future re-ordering of the enum
        // variants would silently flip Ord results.
        let mut sorted = TfIndex::ALL.to_vec();
        sorted.sort();
        assert_eq!(sorted, TfIndex::ALL);
    }
}
