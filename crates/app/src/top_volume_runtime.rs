//! Latest Top Volume boards for RAM-only readers.
//!
//! Publication and daily reset belong to the single, serialized feed drain.
//! Readers may run concurrently. The older-timestamp guard is not a
//! concurrent-writer ordering protocol. Each load is one immutable board;
//! loading several slots does not give an atomic cross-cadence snapshot.
//! Publication validates ordering in O(rows); loading and reading the first
//! row are independent of board length. No database or order-execution I/O.

use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwapOption;
use tickvault_storage::top_volume_rank_persistence::SnapshotCadence;

use crate::top_volume_snapshot::floor_to_grid;
use crate::volume_leaderboard::{
    OptionFamily, RankedContract, compare_ranked_contracts, window_lots_milli,
};

/// One complete, sorted board at an IST snapshot boundary.
#[derive(Debug)]
pub struct TopVolumeSnapshot {
    pub family: OptionFamily,
    pub cadence: SnapshotCadence,
    /// IST wall-clock epoch nanoseconds, matching Top Volume persistence.
    pub ts_nanos: i64,
    pub rows: Vec<RankedContract>,
}

impl TopVolumeSnapshot {
    /// Highest-ranked row, without searching or sorting.
    #[must_use]
    pub fn first(&self) -> Option<&RankedContract> {
        self.rows.first()
    }
}

/// Eight independent latest-board slots: two families and four cadences.
pub struct TopVolumeRuntime {
    slots: [[ArcSwapOption<TopVolumeSnapshot>; SnapshotCadence::ALL.len()]; 2],
}

const fn family_slot(family: OptionFamily) -> usize {
    match family {
        OptionFamily::Index => 0,
        OptionFamily::Stock => 1,
    }
}

impl Default for TopVolumeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl TopVolumeRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| std::array::from_fn(|_| ArcSwapOption::empty())),
        }
    }

    /// Publish the complete output of one ranking pass, including an empty
    /// board. Call only from the serialized drain that also performs reset.
    ///
    /// Rejects nonpositive/off-grid timestamps, invalid volume inputs or unsorted rows,
    /// and timestamps older than the current slot. Same-boundary corrections
    /// are allowed. The ranking producer owns identity uniqueness and board
    /// completeness; this check does not reconstruct either from the feed.
    #[must_use]
    pub fn publish(
        &self,
        family: OptionFamily,
        cadence: SnapshotCadence,
        ts_nanos: i64,
        rows: Vec<RankedContract>,
    ) -> Option<Arc<TopVolumeSnapshot>> {
        let snapshot = Arc::new(TopVolumeSnapshot {
            family,
            cadence,
            ts_nanos,
            rows,
        });
        self.publish_snapshot(Arc::clone(&snapshot))
            .then_some(snapshot)
    }

    /// Publish shared rows while allowing the caller to retain them for other
    /// consumers even if this latest-only cache refuses their timestamp.
    /// The same single-writer and validation contract as `publish` applies.
    #[must_use]
    pub fn publish_snapshot(&self, snapshot: Arc<TopVolumeSnapshot>) -> bool {
        let Some(index) = snapshot.cadence.slot() else {
            return false;
        };
        let ts_nanos = snapshot.ts_nanos;
        if ts_nanos <= 0 || floor_to_grid(ts_nanos, snapshot.cadence.interval_secs()) != ts_nanos {
            return false;
        }
        let rows = &snapshot.rows;
        if rows.iter().any(|row| {
            row.delta_units == 0
                || window_lots_milli(row.delta_units, row.lot_size) != Some(row.window_lots_milli)
        }) || rows
            .windows(2)
            .any(|pair| compare_ranked_contracts(&pair[0], &pair[1]).is_gt())
        {
            return false;
        }
        let slot = &self.slots[family_slot(snapshot.family)][index];
        if slot
            .load()
            .as_ref()
            .is_some_and(|old| old.ts_nanos > ts_nanos)
        {
            return false;
        }
        slot.store(Some(snapshot));
        true
    }

    /// Load a board, retaining its explicit empty state and timestamp.
    #[must_use]
    pub fn load(
        &self,
        family: OptionFamily,
        cadence: SnapshotCadence,
    ) -> Option<Arc<TopVolumeSnapshot>> {
        let index = cadence.slot()?;
        self.slots[family_slot(family)][index].load_full()
    }

    /// Load a nonempty board whose age is in [0, max_age_ns], inclusive.
    /// Both clocks must use the same IST epoch convention. Invalid negative
    /// age limits, future boards and expired boards return None.
    #[must_use]
    pub fn load_fresh(
        &self,
        family: OptionFamily,
        cadence: SnapshotCadence,
        now_ns: i64,
        max_age_ns: i64,
    ) -> Option<Arc<TopVolumeSnapshot>> {
        if max_age_ns < 0 {
            return None;
        }
        let snapshot = self.load(family, cadence)?;
        let age = now_ns.checked_sub(snapshot.ts_nanos)?;
        if snapshot.rows.is_empty() || age < 0 || age > max_age_ns {
            return None;
        }
        Some(snapshot)
    }

    /// Clear all slots from the same serialized owner that publishes boards.
    /// Previously loaded Arcs remain valid immutable historical snapshots.
    /// Clearing the eight slots is not an atomic cross-slot transaction.
    pub fn reset_daily(&self) {
        for family in &self.slots {
            for slot in family {
                slot.store(None);
            }
        }
    }
}

/// Process-local view shared by the drain and RAM readers.
pub fn global_top_volume_runtime() -> &'static TopVolumeRuntime {
    static RUNTIME: OnceLock<TopVolumeRuntime> = OnceLock::new();
    RUNTIME.get_or_init(TopVolumeRuntime::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    const SECOND: i64 = 1_000_000_000;
    // Whole minute, so also on every supported second-scale grid.
    const TS: i64 = 1_800_000_000 * SECOND;

    fn row(id: u64, score: u64) -> RankedContract {
        RankedContract {
            security_id: id,
            segment: ExchangeSegment::NseFno,
            underlying_id: id,
            volume: 100,
            window_lots_milli: score,
            delta_units: u32::try_from(score).expect("bounded test score"),
            lot_size: 1000,
        }
    }

    #[test]
    fn explicit_empty_replaces_a_nonempty_board() {
        let store = TopVolumeRuntime::new();
        let family = OptionFamily::Stock;
        let cadence = SnapshotCadence::OneSecond;
        assert!(
            store
                .publish(family, cadence, TS, vec![row(1, 1000)])
                .is_some()
        );
        assert!(
            store
                .publish(family, cadence, TS + SECOND, vec![])
                .is_some()
        );
        let empty = store.load(family, cadence).expect("explicit empty");
        assert_eq!(empty.ts_nanos, TS + SECOND);
        assert!(empty.first().is_none());
        assert!(
            store
                .load_fresh(family, cadence, TS + SECOND, SECOND)
                .is_none()
        );
    }

    #[test]
    fn freshness_rejects_stale_future_invalid_and_missing_boards() {
        let store = TopVolumeRuntime::new();
        let family = OptionFamily::Index;
        let cadence = SnapshotCadence::FiveSecond;
        assert!(store.load_fresh(family, cadence, TS, SECOND).is_none());
        assert!(
            store
                .publish(family, cadence, TS, vec![row(1, 1000)])
                .is_some()
        );
        assert!(store.load_fresh(family, cadence, TS, 0).is_some());
        assert!(
            store
                .load_fresh(family, cadence, TS + SECOND, SECOND)
                .is_some()
        );
        for (now, limit) in [
            (TS - 1, SECOND),
            (TS + SECOND + 1, SECOND),
            (TS, -1),
            (i64::MIN, 0),
        ] {
            assert!(store.load_fresh(family, cadence, now, limit).is_none());
        }
    }

    #[test]
    fn cadence_grid_and_timestamp_order_are_enforced() {
        let store = TopVolumeRuntime::new();
        for cadence in SnapshotCadence::ALL {
            assert!(
                store
                    .publish(OptionFamily::Stock, cadence, 0, vec![])
                    .is_none()
            );
            assert!(
                store
                    .publish(OptionFamily::Stock, cadence, TS + 1, vec![])
                    .is_none()
            );
            assert!(
                store
                    .publish(OptionFamily::Stock, cadence, TS, vec![row(1, 1000)])
                    .is_some()
            );
            let period = i64::try_from(cadence.interval_secs()).expect("bounded cadence") * SECOND;
            assert!(
                store
                    .publish(OptionFamily::Stock, cadence, TS - period, vec![])
                    .is_none()
            );
            assert!(
                store
                    .publish(OptionFamily::Stock, cadence, TS, vec![row(2, 1000)])
                    .is_some()
            );
            assert_eq!(
                store
                    .load(OptionFamily::Stock, cadence)
                    .expect("board")
                    .first()
                    .expect("row")
                    .security_id,
                2
            );
        }
    }

    #[test]
    fn every_family_and_cadence_is_isolated_and_daily_reset_clears_all() {
        let store = TopVolumeRuntime::new();
        for family in [OptionFamily::Index, OptionFamily::Stock] {
            for cadence in SnapshotCadence::ALL {
                let id =
                    u64::try_from(family_slot(family) * 10 + cadence.index()).expect("small id");
                assert!(
                    store
                        .publish(family, cadence, TS, vec![row(id, 1000)])
                        .is_some()
                );
            }
        }
        for family in [OptionFamily::Index, OptionFamily::Stock] {
            for cadence in SnapshotCadence::ALL {
                let loaded = store.load(family, cadence).expect("own board");
                assert_eq!((loaded.family, loaded.cadence), (family, cadence));
                assert_eq!(
                    loaded.first().expect("row").security_id,
                    u64::try_from(family_slot(family) * 10 + cadence.index()).expect("small id")
                );
            }
        }
        store.reset_daily();
        for family in [OptionFamily::Index, OptionFamily::Stock] {
            for cadence in SnapshotCadence::ALL {
                assert!(store.load(family, cadence).is_none());
            }
        }
    }

    #[test]
    fn previously_loaded_snapshot_survives_replacement_and_reset() {
        let store = TopVolumeRuntime::new();
        let family = OptionFamily::Index;
        let cadence = SnapshotCadence::OneMinute;
        assert!(
            store
                .publish(family, cadence, TS, vec![row(1, 1000)])
                .is_some()
        );
        let held = store.load(family, cadence).expect("held");
        assert!(
            store
                .publish(family, cadence, TS + 60 * SECOND, vec![row(2, 2000)])
                .is_some()
        );
        store.reset_daily();
        assert_eq!(held.ts_nanos, TS);
        assert_eq!(held.first().expect("held row").security_id, 1);
    }

    #[test]
    fn invalid_order_or_zero_score_cannot_replace_a_valid_board() {
        let store = TopVolumeRuntime::new();
        let family = OptionFamily::Stock;
        let cadence = SnapshotCadence::OneSecond;
        assert!(
            store
                .publish(family, cadence, TS, vec![row(1, 2000), row(2, 1000)])
                .is_some()
        );
        for rows in [
            vec![row(1, 1000), row(2, 2000)],
            vec![row(2, 1000), row(1, 1000)],
            vec![row(1, 0)],
        ] {
            assert!(store.publish(family, cadence, TS + SECOND, rows).is_none());
        }
        assert_eq!(store.load(family, cadence).expect("original").ts_nanos, TS);
    }

    #[test]
    fn exact_ratio_order_and_positive_sub_milli_volume_are_preserved() {
        let store = TopVolumeRuntime::new();
        let family = OptionFamily::Stock;
        for cadence in SnapshotCadence::ALL {
            // Both display as zero milli-lots, but 1/2001 is larger than
            // 1/3001. Security ID must not reverse their true order.
            let mut higher = row(9, 0);
            higher.delta_units = 1;
            higher.lot_size = 2001;
            let mut lower = row(1, 0);
            lower.delta_units = 1;
            lower.lot_size = 3001;
            assert!(
                store
                    .publish(family, cadence, TS, vec![higher, lower])
                    .is_some()
            );
            assert!(
                store
                    .publish(family, cadence, TS, vec![lower, higher])
                    .is_none()
            );
            assert_eq!(
                store
                    .load(family, cadence)
                    .unwrap()
                    .first()
                    .unwrap()
                    .security_id,
                9
            );
        }
    }

    #[test]
    fn invalid_denominator_or_inconsistent_display_cannot_replace_a_board() {
        let store = TopVolumeRuntime::new();
        let family = OptionFamily::Index;
        let cadence = SnapshotCadence::OneSecond;
        assert!(
            store
                .publish(family, cadence, TS, vec![row(1, 1000)])
                .is_some()
        );
        let mut invalid_lot = row(2, 1000);
        invalid_lot.lot_size = 0;
        let mut invalid_display = row(2, 1000);
        invalid_display.window_lots_milli = 1001;
        for invalid in [invalid_lot, invalid_display] {
            assert!(
                store
                    .publish(family, cadence, TS + SECOND, vec![invalid])
                    .is_none()
            );
        }
        assert_eq!(store.load(family, cadence).unwrap().ts_nanos, TS);
    }

    /// Local-store measurements only: does not publish into the live global.
    #[test]
    #[ignore = "timing harness; run explicitly in release mode"]
    fn benchmark_runtime_snapshot_reads_and_publication() {
        use std::hint::black_box;
        use std::time::Instant;

        const SAMPLES: usize = 100;
        const READS_PER_BATCH: u128 = 10_000;
        let family = OptionFamily::Stock;
        let cadence = SnapshotCadence::OneSecond;
        for row_count in [1_usize, 25_000] {
            let store = TopVolumeRuntime::new();
            let fixture: Vec<_> = (0..row_count)
                .map(|id| row(u64::try_from(id).expect("bounded id"), 1000))
                .collect();
            let accepted = store
                .publish(family, cadence, TS, fixture.clone())
                .expect("sorted fixture");
            assert_eq!(accepted.rows.len(), row_count);
            drop(accepted);
            let mut reads = Vec::with_capacity(SAMPLES);
            for _ in 0..SAMPLES {
                let start = Instant::now();
                for _ in 0..READS_PER_BATCH {
                    let loaded = store
                        .load_fresh(
                            black_box(family),
                            black_box(cadence),
                            black_box(TS),
                            black_box(SECOND),
                        )
                        .expect("fresh board");
                    black_box(loaded.first().expect("candidate").security_id);
                }
                reads.push(start.elapsed().as_nanos() / READS_PER_BATCH);
            }
            reads.sort_unstable();
            println!(
                "TV_READ rows={row_count} samples={SAMPLES} reads_per_batch={READS_PER_BATCH} \
                p50_batch_mean_ns={} p99_batch_mean_ns={} max_batch_mean_ns={} \
                note=batch_averages_not_individual_access_percentiles",
                reads[49], reads[98], reads[99]
            );

            if row_count == 25_000 {
                let mut publications = Vec::with_capacity(SAMPLES);
                for _ in 0..SAMPLES {
                    // Copy/allocate the full Vec before timing. Publication
                    // includes validation, Arc creation, swap and old release.
                    let prepared = fixture.clone();
                    let start = Instant::now();
                    let published = store
                        .publish(family, cadence, TS, prepared)
                        .expect("same-boundary update");
                    black_box(&published);
                    publications.push(start.elapsed().as_nanos());
                    drop(published);
                }
                publications.sort_unstable();
                println!(
                    "TV_PUBLISH rows={row_count} samples={SAMPLES} \
                    p50_ns={} p99_ns={} max_ns={} \
                    includes=ordering_validation_arc_allocation_swap_old_release \
                    excludes=full_vec_copy_and_fixture_preparation complexity=O(rows)",
                    publications[49], publications[98], publications[99]
                );
            }
        }
    }
}
