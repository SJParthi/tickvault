//! Single-owner adapter from canonical candle publications to exact rankings.
//!
//! This module never reads a tick volume counter or queries a database.
//! Metadata registration is O(N) cold work and warm candle index updates are
//! expected O(F log N), with F = four admitted Top Volume frames. The other
//! six candle frames do not enter this index. Full table materialization is
//! O(N) per family/frame on the maintenance cadence; a complete sweep is
//! O(F * N_total) and still runs
//! on the ingestion owner. Prepared winner reads examine at most eight headers
//! through the published runtime, without scanning contracts.

use std::sync::Arc;

use tickvault_common::feed::Feed;
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::candles::{
    CandleMetadata, CandleVolumeUpdate, TOP_VOLUME_TF_COUNT, TfIndex,
};

use crate::bucket_top_volume::{
    BucketContract, BucketTopVolumeEngine, BucketTopVolumeRuntime, BucketVolumeMetric,
};
use crate::contract_underlying_map::{ContractSnapshot, ContractUnderlyingMap};
use crate::volume_leaderboard::OptionFamily;

/// A fold can publish a prior-session seal, a rebase, a seal/amendment and a
/// current state for each admitted Top Volume frame. Candle-only updates are
/// filtered before insertion. Overflow is a visible invalidation, never an
/// unchecked push or silent truncation.
pub type CandleUpdateBatch = arrayvec::ArrayVec<CandleVolumeUpdate, { TOP_VOLUME_TF_COUNT * 5 }>;

pub struct CandleVolumeBridge {
    definitions: Arc<ContractSnapshot>,
    engine: Option<BucketTopVolumeEngine>,
    pending_winners: [[bool; TOP_VOLUME_TF_COUNT]; 2],
    refused: u64,
    faulted: bool,
    session_day: u32,
}

impl Default for CandleVolumeBridge {
    fn default() -> Self {
        Self {
            definitions: Arc::new(ContractSnapshot::default()),
            engine: None,
            pending_winners: [[false; TOP_VOLUME_TF_COUNT]; 2],
            refused: 0,
            faulted: false,
            session_day: 0,
        }
    }
}

impl CandleVolumeBridge {
    /// Called at attach/maintenance, never to build a universe inside a fold.
    pub fn refresh(
        &mut self,
        now_ist_secs: u32,
        map: &ContractUnderlyingMap,
        runtime: &BucketTopVolumeRuntime,
    ) {
        let next = map.versioned_snapshot();
        let day = now_ist_secs / 86_400;
        if next.version == self.definitions.version && self.session_day == day {
            return;
        }
        runtime.reset();
        self.pending_winners = [[false; TOP_VOLUME_TF_COUNT]; 2];
        if self.session_day != day {
            self.faulted = false;
        }
        self.session_day = day;
        let missing_metadata = next
            .ranked
            .iter()
            .filter(|key| {
                !next.owners.contains_key(key) || !next.definition_versions.contains_key(key)
            })
            .count();
        if missing_metadata != 0 {
            tracing::error!(
                missing_metadata,
                source = "candle_ranking_metadata_incomplete",
                "selected options lack metadata; refusing a falsely complete ranking"
            );
        }
        self.engine = if next.version == 0 || next.ranked.is_empty() || missing_metadata != 0 {
            None
        } else {
            BucketTopVolumeEngine::new(
                Feed::Dhan,
                day,
                next.version,
                BucketVolumeMetric::SignedBarVolumeVsOneLotV3,
                next.ranked.iter().filter_map(|&(security_id, segment)| {
                    let owner = next.owners.get(&(security_id, segment))?;
                    Some(BucketContract {
                        security_id,
                        segment,
                        underlying_id: owner.underlying_id,
                        family: owner.family,
                        lot_size: owner.lot_size,
                        instrument_definition_version: next
                            .definition_versions
                            .get(&(security_id, segment))
                            .copied()
                            .unwrap_or(0),
                    })
                }),
                2,
            )
            .map_err(|error| {
                tracing::error!(
                    ?error,
                    source = "candle_ranking_registration_refused",
                    "canonical candle ranking universe could not be registered"
                );
            })
            .ok()
        };
        self.definitions = next;
    }

    /// Metadata and ranking registration originate from the same publication.
    /// No labels, heap allocation or second volume counter are involved.
    #[must_use]
    pub fn metadata(&self, security_id: u64, segment_code: u8) -> CandleMetadata {
        let Some(segment) = ExchangeSegment::from_byte(segment_code) else {
            return CandleMetadata::UNKNOWN;
        };
        let Some(owner) = self.definitions.owners.get(&(security_id, segment)) else {
            return CandleMetadata::UNKNOWN;
        };
        CandleMetadata {
            lot_size: owner.lot_size,
            instrument_definition_version: self
                .definitions
                .definition_versions
                .get(&(security_id, segment))
                .copied()
                .unwrap_or(0),
            underlying_id: owner.underlying_id,
            family_code: match owner.family {
                OptionFamily::Stock => 1,
                OptionFamily::Index => 2,
            },
        }
    }

    pub fn invalidate(&mut self, runtime: &BucketTopVolumeRuntime, reason: &'static str) {
        self.faulted = true;
        runtime.reset();
        self.refused = self.refused.saturating_add(1);
        metrics::counter!("tv_candle_ranking_refused_total", "reason" => reason).increment(1);
        tracing::error!(
            source = "candle_ranking_invalidated",
            reason,
            "ranking is blocked for the current session; metadata refresh does not clear the fault"
        );
    }

    /// Apply the exact state provided by the candle owner, including signed
    /// quantity, pinned multiplier, quality flags and revision.
    pub fn apply(&mut self, update: CandleVolumeUpdate) {
        let Some(slot) = update.tf.top_volume_ordinal() else {
            return;
        };
        if self.faulted {
            return;
        }
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        // Prior-session seals are historical writes, never live candidates.
        if update.session_day != engine.session_day() {
            return;
        }
        if engine
            .family_of(update.security_id, update.segment_code)
            .is_none()
        {
            return;
        }
        // Buckets/retention are shared by the two families. A new timestamp
        // can evict an old bucket from both, even when only one family ticked.
        // Publish their bounded winner windows together after the whole batch;
        // unchanged entries keep their existing revision and clock.
        self.pending_winners[0][slot] = true;
        self.pending_winners[1][slot] = true;
        if let Err(error) = engine.apply(&update) {
            self.refused = self.refused.saturating_add(1);
            metrics::counter!("tv_candle_ranking_refused_total", "reason" => "canonical_update")
                .increment(1);
            if self.refused.is_power_of_two() {
                tracing::warn!(
                    ?error,
                    refused = self.refused,
                    source = "candle_ranking_update_refused",
                    "canonical candle update was refused; coverage remains explicit"
                );
            }
        }
    }

    /// Publish each touched frame's retained winner window after the whole
    /// tick batch was accepted. A seal sweep calls this once after its sweep.
    pub fn publish_winners(&mut self, now_ist_secs: u32, runtime: &BucketTopVolumeRuntime) {
        if self.faulted {
            return;
        }
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let mut failed = false;
        for (slot, family) in [(0, OptionFamily::Stock), (1, OptionFamily::Index)] {
            for (ordinal, tf) in TfIndex::TOP_VOLUME_ALL.into_iter().enumerate() {
                if !std::mem::take(&mut self.pending_winners[slot][ordinal]) {
                    continue;
                }
                if let Err(error) = engine.publish_winner_latest(runtime, family, tf, now_ist_secs)
                {
                    failed = true;
                    metrics::counter!("tv_candle_ranking_refused_total", "reason" => "winner_publication").increment(1);
                    tracing::warn!(
                        ?error,
                        source = "candle_winner_publication_refused",
                        "canonical winner publication was refused"
                    );
                }
            }
        }
        if failed {
            self.invalidate(runtime, "winner_publication");
        }
    }

    /// Full sorted tables are copied only when changed, on the maintenance
    /// path. No tick counter is read and no baseline is rolled here.
    pub fn publish_tables(
        &self,
        now_ist_secs: u32,
        runtime: &BucketTopVolumeRuntime,
    ) -> (usize, usize) {
        if self.faulted {
            return (0, 1);
        }
        let Some(engine) = self.engine.as_ref() else {
            return (0, 0);
        };
        let mut rows = 0usize;
        let mut refused = 0usize;
        for family in [OptionFamily::Stock, OptionFamily::Index] {
            for tf in TfIndex::TOP_VOLUME_ALL {
                match engine.publish_latest(runtime, family, tf, now_ist_secs) {
                    Ok(Some(snapshot)) => rows = rows.saturating_add(snapshot.rows.len()),
                    Ok(None) => {}
                    Err(_) => refused = refused.saturating_add(1),
                }
            }
        }
        (rows, refused)
    }

    pub fn reset(&mut self, runtime: &BucketTopVolumeRuntime) {
        let faulted = self.faulted;
        let session_day = self.session_day;
        *self = Self::default();
        self.faulted = faulted;
        self.session_day = session_day;
        runtime.reset();
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.definitions.version
    }

    /// Revoke an old publication immediately if the attach owner has changed
    /// the declared universe. Rebuilding the new engine remains cold work.
    pub fn require_current_metadata(
        &self,
        map: &ContractUnderlyingMap,
        runtime: &BucketTopVolumeRuntime,
    ) -> bool {
        if self.definitions.version != map.current_version() {
            runtime.reset();
            false
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket_top_volume::{BucketDecisionRefusal, BucketDecisionRequest};
    use crate::contract_underlying_map::LegIds;
    use tickvault_common::tick_types::VolumeQuality;
    use tickvault_trading::candles::TF_COUNT;

    const DAY: u32 = 20_000;
    const NOW: u32 = DAY * 86_400 + 34_000;

    fn leg(id: u64, family: OptionFamily, lot: u32) -> LegIds {
        LegIds {
            contract_security_id: i64::try_from(id).unwrap(),
            underlying_security_id: i64::try_from(id + 1_000).unwrap(),
            contract_segment: ExchangeSegment::NseFno,
            family,
            lot_size: lot,
        }
    }

    fn setup(
        legs: &[LegIds],
    ) -> (
        ContractUnderlyingMap,
        CandleVolumeBridge,
        BucketTopVolumeRuntime,
    ) {
        let map = ContractUnderlyingMap::new();
        map.publish_from_legs(legs);
        let mut bridge = CandleVolumeBridge::default();
        let runtime = BucketTopVolumeRuntime::new();
        bridge.refresh(NOW, &map, &runtime);
        (map, bridge, runtime)
    }

    fn observation(
        bridge: &CandleVolumeBridge,
        id: u64,
        net: Option<i64>,
        revision: u64,
    ) -> CandleVolumeUpdate {
        let tf = TfIndex::S5;
        CandleVolumeUpdate {
            feed: Feed::Dhan,
            security_id: id,
            segment_code: ExchangeSegment::NseFno.binary_code(),
            tf,
            session_day: DAY,
            bucket_start_secs: tf.bucket_start(NOW),
            bucket_end_secs: tf.bucket_end(tf.bucket_start(NOW)),
            gross_volume: net.map_or(500, |value| {
                if value == 0 {
                    500
                } else {
                    value.unsigned_abs()
                }
            }),
            estimated_net_volume: net,
            metadata: bridge.metadata(id, ExchangeSegment::NseFno.binary_code()),
            volume_quality: 0,
            revision,
            last_observed_secs: NOW,
            quality: VolumeQuality {
                baseline_known: true,
                counter_ambiguous: false,
                volume_missing: false,
                attribution_uncertain: false,
            },
            closed: false,
        }
    }

    fn read_request(
        bridge: &CandleVolumeBridge,
        family: OptionFamily,
        now: u32,
    ) -> BucketDecisionRequest {
        BucketDecisionRequest {
            feed: Feed::Dhan,
            family,
            tf: TfIndex::S5,
            session_day: DAY,
            universe_version: bridge.definitions.version,
            metric: BucketVolumeMetric::SignedBarVolumeVsOneLotV3,
            bucket_start_secs: TfIndex::S5.bucket_start(NOW),
            now_secs: now,
            max_age_secs: 5,
            require_closed: false,
        }
    }

    #[test]
    fn canonical_metadata_and_signed_values_reach_winner_before_any_table_materialization() {
        let (_, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
        let update = observation(&bridge, 1, Some(-250), 1);
        bridge.apply(update);
        bridge.publish_winners(NOW, &runtime);
        let winner = runtime
            .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
            .unwrap();
        let row = winner.first().unwrap();
        assert_eq!(row.gross_volume, update.gross_volume);
        assert_eq!(row.estimated_net_volume, update.estimated_net_volume);
        assert_eq!(row.lot_size, update.metadata.lot_size);
        assert_eq!(
            row.instrument_definition_version,
            update.metadata.instrument_definition_version
        );
        assert_eq!(row.score_milli_pct(winner.metric), Some(-350_000));
        assert!(runtime.load(OptionFamily::Stock, TfIndex::S5).is_none());
        let (rows, refused) = bridge.publish_tables(NOW, &runtime);
        assert_eq!((rows, refused), (1, 0));
        let full = runtime
            .load_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
            .unwrap();
        assert_eq!(full.first(), winner.first());
    }

    #[test]
    fn both_family_slots_are_independent_and_zero_net_is_a_real_value() {
        let (_, mut bridge, runtime) = setup(&[
            leg(1, OptionFamily::Stock, 100),
            leg(2, OptionFamily::Index, 25),
        ]);
        bridge.apply(observation(&bridge, 1, Some(0), 1));
        bridge.apply(observation(&bridge, 2, Some(-500), 1));
        bridge.publish_winners(NOW, &runtime);
        for (family, id) in [(OptionFamily::Stock, 1), (OptionFamily::Index, 2)] {
            let winner = runtime
                .load_winner_for_decision(read_request(&bridge, family, NOW))
                .unwrap();
            assert_eq!(winner.first().unwrap().security_id, id);
            assert_eq!(winner.coverage.expected_contracts, 1);
        }
    }

    #[test]
    fn ten_candle_frames_produce_only_eight_admitted_top_volume_family_windows() {
        let (_, mut bridge, runtime) = setup(&[
            leg(1, OptionFamily::Stock, 100),
            leg(2, OptionFamily::Index, 25),
        ]);
        for tf in TfIndex::ALL {
            for (id, net) in [(1, 150), (2, -50)] {
                let mut candle = observation(&bridge, id, Some(net), 1);
                candle.tf = tf;
                candle.bucket_start_secs = tf.bucket_start(NOW);
                candle.bucket_end_secs = tf.bucket_end(candle.bucket_start_secs);
                bridge.apply(candle);
            }
        }
        bridge.publish_winners(NOW, &runtime);
        let mut windows = 0;
        for tf in TfIndex::ALL {
            for (family, id, net, lot) in [
                (OptionFamily::Stock, 1, 150, 100),
                (OptionFamily::Index, 2, -50, 25),
            ] {
                if !tf.is_top_volume() {
                    assert!(runtime.load(family, tf).is_none());
                    assert!(runtime.load_winner(family, tf).is_none());
                    assert!(
                        bridge
                            .engine
                            .as_ref()
                            .unwrap()
                            .latest_bucket_start(tf)
                            .is_none()
                    );
                    continue;
                }
                let mut read = read_request(&bridge, family, NOW);
                read.tf = tf;
                read.bucket_start_secs = tf.bucket_start(NOW);
                let winner = runtime.load_winner_for_decision(read).unwrap();
                assert_eq!(winner.tf, tf);
                assert_eq!(winner.family, family);
                assert_eq!(winner.first().unwrap().security_id, id);
                assert_eq!(winner.first().unwrap().estimated_net_volume, Some(net));
                assert_eq!(winner.first().unwrap().lot_size, lot);
                assert_eq!(winner.coverage.expected_contracts, 1);
                assert!(runtime.load(family, tf).is_none());
                windows += 1;
            }
        }
        assert_eq!((TF_COUNT, TOP_VOLUME_TF_COUNT, windows), (10, 4, 8));
        assert_eq!(CandleUpdateBatch::new().capacity(), 20);
        assert_eq!(
            bridge.refused, 0,
            "candle-only frames are intentionally filtered"
        );
        assert!(
            bridge
                .pending_winners
                .iter()
                .flatten()
                .all(|pending| !pending)
        );
        assert_eq!(bridge.publish_tables(NOW, &runtime), (8, 0));
        for tf in TfIndex::TOP_VOLUME_ALL {
            for family in [OptionFamily::Stock, OptionFamily::Index] {
                assert_eq!(
                    runtime.load(family, tf).unwrap().first(),
                    runtime.load_winner(family, tf).unwrap().first()
                );
            }
        }
        bridge.reset(&runtime);
        for tf in TfIndex::ALL {
            for family in [OptionFamily::Stock, OptionFamily::Index] {
                assert!(runtime.load(family, tf).is_none());
                assert!(runtime.load_winner(family, tf).is_none());
            }
        }
    }

    #[test]
    fn candle_only_updates_do_not_change_active_winners_or_refresh_their_clocks() {
        let (_, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.publish_winners(NOW, &runtime);
        let original = runtime
            .load_winner(OptionFamily::Stock, TfIndex::S5)
            .unwrap();
        for tf in TfIndex::ALL.into_iter().filter(|tf| !tf.is_top_volume()) {
            let mut excluded = observation(&bridge, 1, Some(200), 99);
            excluded.tf = tf;
            excluded.metadata = CandleMetadata::UNKNOWN;
            excluded.bucket_start_secs = 0;
            bridge.apply(excluded);
            bridge.publish_winners(NOW + 30, &runtime);
            assert!(runtime.load(OptionFamily::Stock, tf).is_none());
            assert!(runtime.load_winner(OptionFamily::Stock, tf).is_none());
            let unchanged = runtime
                .load_winner(OptionFamily::Stock, TfIndex::S5)
                .unwrap();
            assert!(Arc::ptr_eq(&original, &unchanged));
            assert_eq!(unchanged.published_secs, NOW);
        }
        assert_eq!(bridge.refused, 0);
        assert!(
            bridge
                .pending_winners
                .iter()
                .flatten()
                .all(|pending| !pending)
        );
    }

    #[test]
    fn rollover_publishes_closed_predecessors_and_evicts_them_for_both_families() {
        let (_, mut bridge, runtime) = setup(&[
            leg(1, OptionFamily::Stock, 100),
            leg(2, OptionFamily::Index, 25),
        ]);
        for (id, net) in [(1, 100), (2, -50)] {
            let mut closed = observation(&bridge, id, Some(net), 1);
            closed.closed = true;
            closed.last_observed_secs = NOW + 5;
            bridge.apply(closed);
        }
        let mut live = observation(&bridge, 1, Some(50), 2);
        live.bucket_start_secs += 5;
        live.bucket_end_secs += 5;
        live.last_observed_secs += 5;
        bridge.apply(live);
        bridge.publish_winners(NOW + 5, &runtime);
        for family in [OptionFamily::Stock, OptionFamily::Index] {
            let mut read = read_request(&bridge, family, NOW + 5);
            read.require_closed = true;
            let previous = runtime.load_winner_for_decision(read).unwrap();
            assert_eq!(previous.bucket_start_secs, NOW);
            assert!(previous.first().unwrap().closed);
        }
        let mut current = read_request(&bridge, OptionFamily::Stock, NOW + 5);
        current.bucket_start_secs += 5;
        current.require_closed = true;
        assert_eq!(
            runtime.load_winner_for_decision(current).unwrap_err(),
            BucketDecisionRefusal::OpenBucket
        );
        current.require_closed = false;
        assert_eq!(
            runtime
                .load_winner_for_decision(current)
                .unwrap()
                .first()
                .unwrap()
                .estimated_net_volume,
            Some(50)
        );
        // Only stock ticks again. Shared engine eviction must still remove
        // the index family's now-expired historical winner publication.
        live.bucket_start_secs += 5;
        live.bucket_end_secs += 5;
        live.last_observed_secs += 5;
        live.revision += 1;
        bridge.apply(live);
        bridge.publish_winners(NOW + 10, &runtime);
        for family in [OptionFamily::Stock, OptionFamily::Index] {
            let mut read = read_request(&bridge, family, NOW + 10);
            read.require_closed = true;
            assert_eq!(
                runtime.load_winner_for_decision(read).unwrap_err(),
                BucketDecisionRefusal::DifferentBucket
            );
        }
        bridge.invalidate(&runtime, "tick_writer_refusal");
        assert!(
            runtime
                .load_winner(OptionFamily::Stock, TfIndex::S5)
                .is_none()
        );
        assert!(
            runtime
                .load_winner(OptionFamily::Index, TfIndex::S5)
                .is_none()
        );
    }

    #[test]
    fn a_missing_registered_contract_is_not_removed_from_the_coverage_denominator() {
        let (_, mut bridge, runtime) = setup(&[
            leg(1, OptionFamily::Stock, 100),
            leg(2, OptionFamily::Stock, 100),
        ]);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.publish_winners(NOW, &runtime);
        assert_eq!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .unwrap_err(),
            BucketDecisionRefusal::IncompleteUniverse
        );
        let snapshot = runtime
            .load_winner(OptionFamily::Stock, TfIndex::S5)
            .unwrap();
        assert_eq!(
            (
                snapshot.coverage.expected_contracts,
                snapshot.coverage.observed_contracts
            ),
            (2, 1)
        );
        bridge.apply(observation(&bridge, 2, Some(0), 1));
        bridge.publish_winners(NOW, &runtime);
        assert!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .is_ok()
        );
    }

    #[test]
    fn missing_or_ambiguous_net_replaces_the_previously_usable_winner() {
        let (_, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.publish_winners(NOW, &runtime);
        assert!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .is_ok()
        );
        let mut ambiguous = observation(&bridge, 1, None, 2);
        ambiguous.quality.counter_ambiguous = true;
        ambiguous.volume_quality =
            tickvault_trading::candles::volume_update::VOLUME_QUALITY_COUNTER_AMBIGUOUS;
        bridge.apply(ambiguous);
        bridge.publish_winners(NOW, &runtime);
        assert_eq!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .unwrap_err(),
            BucketDecisionRefusal::IncompleteUniverse
        );
        let snapshot = runtime
            .load_winner(OptionFamily::Stock, TfIndex::S5)
            .unwrap();
        assert!(snapshot.first().is_none());
        assert_eq!(snapshot.coverage.unavailable_net_contracts, 1);
    }

    #[test]
    fn a_changed_multiplier_cannot_be_applied_to_a_preexisting_candle_numerator() {
        let (_, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.publish_winners(NOW, &runtime);
        let mut incompatible = observation(&bridge, 1, Some(100), 2);
        incompatible.metadata.lot_size = 1;
        bridge.apply(incompatible);
        bridge.publish_winners(NOW, &runtime);
        assert_eq!(bridge.refused, 1);
        let snapshot = runtime
            .load_winner(OptionFamily::Stock, TfIndex::S5)
            .unwrap();
        assert!(snapshot.coverage.integrity_fault);
        assert_eq!(snapshot.first().unwrap().lot_size, 100);
        assert_eq!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .unwrap_err(),
            BucketDecisionRefusal::IncompleteUniverse
        );
    }

    #[test]
    fn stale_instrument_cannot_be_hidden_by_another_instruments_fresh_update() {
        let (_, mut bridge, runtime) = setup(&[
            leg(1, OptionFamily::Stock, 100),
            leg(2, OptionFamily::Stock, 100),
        ]);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.apply(observation(&bridge, 2, Some(10), 1));
        bridge.publish_winners(NOW, &runtime);
        let mut fresh = observation(&bridge, 1, Some(200), 2);
        fresh.last_observed_secs += 6;
        bridge.apply(fresh);
        bridge.publish_winners(NOW + 6, &runtime);
        assert_eq!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW + 6))
                .unwrap_err(),
            BucketDecisionRefusal::StaleInstrument
        );
    }

    #[test]
    fn generation_change_invalidates_old_boards_without_repricing_unchanged_contract_metadata() {
        let (map, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
        let old_metadata = bridge.metadata(1, 2);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.publish_winners(NOW, &runtime);
        let old = runtime
            .load_winner(OptionFamily::Stock, TfIndex::S5)
            .unwrap();
        map.publish_from_legs(&[
            leg(1, OptionFamily::Stock, 100),
            leg(2, OptionFamily::Stock, 25),
        ]);
        bridge.refresh(NOW, &map, &runtime);
        assert!(
            runtime
                .load_winner(OptionFamily::Stock, TfIndex::S5)
                .is_none()
        );
        assert_eq!(bridge.metadata(1, 2), old_metadata);
        assert!(bridge.definitions.version > old.universe_version);
        assert_eq!(old.first().unwrap().lot_size, 100);
        bridge.apply(observation(&bridge, 1, Some(100), 2));
        bridge.publish_winners(NOW, &runtime);
        assert_eq!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .unwrap_err(),
            BucketDecisionRefusal::IncompleteUniverse
        );
    }

    #[test]
    fn explicit_invalidation_stays_closed_until_a_new_session_or_metadata_generation() {
        let (map, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.publish_winners(NOW, &runtime);
        bridge.invalidate(&runtime, "test_batch_overflow");
        bridge.refresh(NOW, &map, &runtime);
        bridge.apply(observation(&bridge, 1, Some(200), 2));
        bridge.publish_winners(NOW, &runtime);
        assert!(
            runtime
                .load_winner(OptionFamily::Stock, TfIndex::S5)
                .is_none()
        );
        assert_eq!(bridge.publish_tables(NOW, &runtime), (0, 1));
        bridge.refresh(NOW + 86_400, &map, &runtime);
        assert!(!bridge.faulted);
        assert_eq!(bridge.engine.as_ref().unwrap().session_day(), DAY + 1);
    }

    #[test]
    fn selected_options_with_missing_metadata_cannot_leave_a_complete_mapped_subset() {
        use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

        // Cover both an absent artifact leg and a leg rejected for a zero lot.
        for rejected_leg_present in [false, true] {
            let (map, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
            bridge.apply(observation(&bridge, 1, Some(100), 1));
            bridge.publish_winners(NOW, &runtime);
            assert!(
                runtime
                    .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                    .is_ok()
            );
            assert_eq!(bridge.publish_tables(NOW, &runtime), (1, 0));

            let selected = [1, 2].map(|security_id| SubscribeInstrument {
                security_id,
                segment: ExchangeSegment::NseFno,
            });
            let mut legs = vec![leg(1, OptionFamily::Stock, 100)];
            if rejected_leg_present {
                legs.push(leg(2, OptionFamily::Stock, 0));
            }
            map.publish_selected_from_legs(&legs, &selected);
            let declared = map.versioned_snapshot();
            assert_eq!(declared.ranked.len(), 2);
            assert!(declared.ranked.contains(&(2, ExchangeSegment::NseFno)));
            assert!(!declared.owners.contains_key(&(2, ExchangeSegment::NseFno)));
            assert!(!bridge.require_current_metadata(&map, &runtime));
            assert!(
                runtime
                    .load_winner(OptionFamily::Stock, TfIndex::S5)
                    .is_none()
            );
            assert!(runtime.load(OptionFamily::Stock, TfIndex::S5).is_none());

            bridge.refresh(NOW, &map, &runtime);
            bridge.apply(observation(&bridge, 1, Some(200), 2));
            bridge.publish_winners(NOW, &runtime);
            assert_eq!(bridge.publish_tables(NOW, &runtime), (0, 0));
            assert_eq!(
                runtime
                    .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                    .unwrap_err(),
                BucketDecisionRefusal::Unavailable
            );

            // Supplying the missing definition restores availability only
            // after both declared members have a canonical observation.
            map.publish_selected_from_legs(
                &[
                    leg(1, OptionFamily::Stock, 100),
                    leg(2, OptionFamily::Stock, 25),
                ],
                &selected,
            );
            bridge.refresh(NOW, &map, &runtime);
            bridge.apply(observation(&bridge, 1, Some(200), 3));
            bridge.apply(observation(&bridge, 2, Some(0), 1));
            bridge.publish_winners(NOW, &runtime);
            let restored = runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .unwrap();
            assert_eq!(restored.coverage.expected_contracts, 2);
            assert_eq!(restored.coverage.observed_contracts, 2);
        }
    }

    #[test]
    fn writer_fault_survives_same_day_metadata_refresh_and_reset_until_a_new_session() {
        let (map, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.publish_winners(NOW, &runtime);
        assert!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .is_ok()
        );
        bridge.invalidate(&runtime, "tick_writer_refusal");

        let previous_generation = map.current_version();
        map.publish_from_legs(&[leg(1, OptionFamily::Stock, 50)]);
        assert!(map.current_version() > previous_generation);
        bridge.refresh(NOW, &map, &runtime);
        bridge.apply(observation(&bridge, 1, Some(200), 2));
        bridge.publish_winners(NOW, &runtime);
        assert_eq!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .unwrap_err(),
            BucketDecisionRefusal::Unavailable
        );
        assert_eq!(bridge.publish_tables(NOW, &runtime), (0, 1));

        bridge.reset(&runtime);
        bridge.refresh(NOW, &map, &runtime);
        bridge.apply(observation(&bridge, 1, Some(300), 3));
        bridge.publish_winners(NOW, &runtime);
        assert_eq!(
            runtime
                .load_winner_for_decision(read_request(&bridge, OptionFamily::Stock, NOW))
                .unwrap_err(),
            BucketDecisionRefusal::Unavailable
        );
        assert_eq!(bridge.publish_tables(NOW, &runtime), (0, 1));

        let next_session = NOW + 86_400;
        bridge.refresh(next_session, &map, &runtime);
        let mut next = observation(&bridge, 1, Some(200), 4);
        next.session_day = DAY + 1;
        next.bucket_start_secs = TfIndex::S5.bucket_start(next_session);
        next.bucket_end_secs = TfIndex::S5.bucket_end(next.bucket_start_secs);
        next.last_observed_secs = next_session;
        bridge.apply(next);
        bridge.publish_winners(next_session, &runtime);
        let mut request = read_request(&bridge, OptionFamily::Stock, next_session);
        request.session_day = next.session_day;
        request.bucket_start_secs = next.bucket_start_secs;
        let restored = runtime.load_winner_for_decision(request).unwrap();
        assert_eq!(restored.first().unwrap().estimated_net_volume, Some(200));
        assert_eq!(restored.first().unwrap().lot_size, 50);
        assert_eq!(bridge.publish_tables(next_session, &runtime), (1, 0));
    }

    #[test]
    fn invalid_publication_revokes_the_previous_winner_and_table_until_recovery() {
        let (_, mut bridge, runtime) = setup(&[leg(1, OptionFamily::Stock, 100)]);
        bridge.apply(observation(&bridge, 1, Some(100), 1));
        bridge.publish_winners(NOW, &runtime);
        let request = read_request(&bridge, OptionFamily::Stock, NOW + 1);
        assert!(runtime.load_winner_for_decision(request).is_ok());
        assert_eq!(bridge.publish_tables(NOW, &runtime), (1, 0));
        assert!(runtime.load_for_decision(request).is_ok());

        let mut newer = observation(&bridge, 1, Some(200), 2);
        newer.last_observed_secs = NOW + 1;
        bridge.apply(newer);
        // An invalid publication clock must revoke the old green revision,
        // even though it would otherwise remain within the freshness limit.
        bridge.publish_winners(NOW, &runtime);
        assert_eq!(
            runtime.load_winner_for_decision(request).unwrap_err(),
            BucketDecisionRefusal::Unavailable
        );
        assert_eq!(
            runtime.load_for_decision(request).unwrap_err(),
            BucketDecisionRefusal::Unavailable
        );

        newer.revision = 3;
        bridge.apply(newer);
        bridge.publish_winners(NOW + 1, &runtime);
        assert_eq!(
            runtime.load_winner_for_decision(request).unwrap_err(),
            BucketDecisionRefusal::Unavailable
        );
        assert_eq!(bridge.publish_tables(NOW + 1, &runtime), (0, 1));
    }
}
