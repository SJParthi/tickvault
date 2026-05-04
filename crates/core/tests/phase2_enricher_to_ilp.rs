//! Phase 2 commit 6 — end-to-end ratchet: TickEnricher → TickLifecycle.
//!
//! Proves the contract between `crates/core/src/pipeline/tick_enricher.rs`
//! and `crates/storage/src/tick_persistence.rs::TickLifecycle`: a tick
//! that flows through the enricher produces lifecycle values whose
//! mapping into the persistence carrier matches what the ILP writer
//! expects.
//!
//! ILP wire-format ratchets live in `tick_persistence.rs::tests`
//! (`test_build_tick_row_enriched_writes_all_lifecycle_columns`) and
//! pin the segment+phase symbol ordering required by QuestDB.
//!
//! Deferred to commit 7: rewiring `run_tick_processor` to drive the
//! enricher in the production SPSC consumer loop. That requires the
//! boot-ordering gate (L14) so the prev_oi_cache is hot before
//! subscribe fires; bundling them keeps the boot-time invariants in
//! one focused PR diff.

use tickvault_common::phase::Phase;
use tickvault_common::tick_types::ParsedTick;

use tickvault_core::pipeline::tick_enricher::TickEnricher;
use tickvault_storage::tick_persistence::TickLifecycle;

/// Helper: map an `EnrichedTick` to the persistence carrier the writer
/// accepts. This is the production wiring contract — a one-liner
/// because every field aligns 1:1 by name.
fn lifecycle_from_enriched(
    e: &tickvault_core::pipeline::tick_enricher::EnrichedTick<'_>,
) -> TickLifecycle {
    TickLifecycle {
        volume_delta: e.volume_delta,
        prev_day_close: e.prev_day_close,
        prev_day_oi: e.prev_day_oi,
        phase: e.phase as u8,
    }
}

/// Phase 2 ratchet: end-to-end enricher → lifecycle carrier mapping.
/// First-seen tick during OPEN phase. Loaded prev_oi_cache supplies
/// the OI value; stamper freezes the prev_day_close from the packet.
#[test]
fn phase2_enricher_to_lifecycle_open_first_seen() {
    let enricher = TickEnricher::new();
    enricher.prev_oi_cache.insert(13, 0, 1_500_000); // NIFTY IDX_I

    let tick = ParsedTick {
        security_id: 13,
        exchange_segment_code: 0,
        last_traded_price: 22_500.5,
        volume: 1_000_000,
        day_close: 22_400.0,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 1_700_000_000_000_000_000,
        ..ParsedTick::default()
    };

    // 09:30 IST = OPEN.
    let now_ist_secs = 9 * 3600 + 30 * 60;
    let enriched = enricher.enrich_tick(&tick, now_ist_secs);
    let life = lifecycle_from_enriched(&enriched);

    assert_eq!(enriched.phase, Phase::Open);
    assert!(enriched.volume_is_first_seen);
    assert!(!enriched.volume_is_regression);
    assert_eq!(life.volume_delta, 1_000_000); // first-seen = full cum
    assert_eq!(life.prev_day_close, 22_400.0);
    assert_eq!(life.prev_day_oi, 1_500_000);
    assert_eq!(life.phase, Phase::Open as u8); // = 2
}

/// Phase 2 ratchet: PREMARKET tick maps to phase=0 (PREMARKET) so the
/// downstream ILP writer emits "phase=PREMARKET" SYMBOL string.
/// VOLUME-MONO-01 suppression during PREMARKET (L13 step 5) is
/// enforced by the consumer using this field.
#[test]
fn phase2_premarket_tick_maps_to_phase_zero() {
    let enricher = TickEnricher::new();
    let tick = ParsedTick {
        security_id: 1234,
        exchange_segment_code: 1,
        volume: 500,
        day_close: 100.0,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 1_700_000_000_000_000_000,
        ..ParsedTick::default()
    };

    let now_ist_secs = 7 * 3600; // 07:00 IST
    let enriched = enricher.enrich_tick(&tick, now_ist_secs);
    let life = lifecycle_from_enriched(&enriched);

    assert_eq!(enriched.phase, Phase::Premarket);
    assert_eq!(life.phase, 0_u8);
    assert!(enriched.volume_is_first_seen);
}

/// Phase 2 ratchet: midnight rollover end-to-end. Day 1 stamps
/// prev_day_close=100; rollover_for_new_day clears state; Day 2
/// stamps prev_day_close=105 from the new packet. Proves the L13
/// invariant flows correctly from enricher state → carrier values.
#[test]
fn phase2_midnight_rollover_clears_stamps_end_to_end() {
    let enricher = TickEnricher::new();

    // Day 1, 09:30 IST OPEN.
    let day1 = ParsedTick {
        security_id: 99,
        exchange_segment_code: 1,
        volume: 10_000,
        day_close: 100.0,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 1_700_000_000_000_000_000,
        ..ParsedTick::default()
    };
    let day1_enriched = enricher.enrich_tick(&day1, 9 * 3600 + 30 * 60);
    let day1_life = lifecycle_from_enriched(&day1_enriched);
    assert_eq!(day1_life.prev_day_close, 100.0);
    assert_eq!(day1_life.phase, Phase::Open as u8);

    // IST midnight rollover (caller drives this from the
    // MidnightRolloverTask).
    enricher.rollover_for_new_day();

    // Day 2 first tick, 00:30 IST PREMARKET, new prev-day-close.
    let day2 = ParsedTick {
        security_id: 99,
        exchange_segment_code: 1,
        volume: 50,       // Dhan reset cumulative
        day_close: 105.0, // new prev-day-close
        exchange_timestamp: 1_700_086_400,
        received_at_nanos: 1_700_086_400_000_000_000,
        ..ParsedTick::default()
    };
    let day2_enriched = enricher.enrich_tick(&day2, 30 * 60);
    let day2_life = lifecycle_from_enriched(&day2_enriched);

    assert_eq!(
        day2_life.prev_day_close, 105.0,
        "midnight rollover must clear stamper so day 2 stamps the new prev-day-close"
    );
    assert_eq!(day2_life.phase, Phase::Premarket as u8);
    assert!(day2_enriched.volume_is_first_seen);
    assert!(
        !day2_enriched.volume_is_regression,
        "L13 day-rollover invariant: post-clear volume reset is NOT a regression"
    );
}

/// Phase 2 ratchet: I-P1-11 composite-key isolation flows end-to-end.
/// Same security_id under different exchange_segment_codes produces
/// independent lifecycle values via the `(sid, segment)` keying.
#[test]
fn phase2_composite_key_isolation_end_to_end() {
    let enricher = TickEnricher::new();
    enricher.prev_oi_cache.insert(13, 0, 1_000_000); // NIFTY IDX_I
    enricher.prev_oi_cache.insert(13, 1, 50_000); // hypothetical NSE_EQ same sid

    let now_ist_secs = 9 * 3600 + 30 * 60;
    let t_idx = ParsedTick {
        security_id: 13,
        exchange_segment_code: 0,
        volume: 1_000,
        day_close: 22500.0,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 1_700_000_000_000_000_000,
        ..ParsedTick::default()
    };
    let t_eq = ParsedTick {
        security_id: 13,
        exchange_segment_code: 1,
        volume: 100,
        day_close: 1500.0,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 1_700_000_000_000_000_000,
        ..ParsedTick::default()
    };
    let r_idx = enricher.enrich_tick(&t_idx, now_ist_secs);
    let r_eq = enricher.enrich_tick(&t_eq, now_ist_secs);
    let life_idx = lifecycle_from_enriched(&r_idx);
    let life_eq = lifecycle_from_enriched(&r_eq);

    assert_eq!(life_idx.prev_day_oi, 1_000_000);
    assert_eq!(life_eq.prev_day_oi, 50_000);
    assert_eq!(life_idx.prev_day_close, 22500.0);
    assert_eq!(life_eq.prev_day_close, 1500.0);
    // Both first-seen (separate composite keys).
    assert!(r_idx.volume_is_first_seen);
    assert!(r_eq.volume_is_first_seen);
}

/// Phase 2 ratchet: every Phase repr-u8 round-trips through the
/// lifecycle carrier without value drift.
#[test]
fn phase2_phase_repr_u8_roundtrip_through_lifecycle() {
    for phase in Phase::all() {
        let life = TickLifecycle {
            volume_delta: 0,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            phase: phase as u8,
        };
        // Reconstruct discriminant value from u8 — pin the exact
        // contract that ParsedTick → TickLifecycle relies on.
        let expected: u8 = match phase {
            Phase::Premarket => 0,
            Phase::Preopen => 1,
            Phase::Open => 2,
            Phase::PostAuction => 3,
            Phase::Closed => 4,
        };
        assert_eq!(life.phase, expected, "phase {:?} round-trip", phase);
    }
}
