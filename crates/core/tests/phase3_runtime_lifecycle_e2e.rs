//! Phase 3 prep — runtime end-to-end lifecycle ratchet.
//!
//! Closes the L4 gap from the hostile bug-hunt review on Phase 2:
//!
//! > "the source-scan ratchets pin the wiring but don't exercise the
//! > actual hot path with a live tick. Could a runtime bug slip past?"
//!
//! This test pumps a synthetic `ParsedTick` through the full
//! production-attached chain — `TickEnricher::enrich_tick` →
//! `TickLifecycle` carrier → `TickPersistenceWriter::append_tick_enriched`
//! — and asserts the writer's `pending_count` advances correctly
//! (proving the row reached the writer's ILP buffer without panic).
//!
//! Unlike the source-scan ratchets that look at SOURCE TEXT of
//! `tick_processor.rs`, this test exercises the runtime data path.
//! A regression that breaks `build_tick_row_enriched`, the
//! `EnrichedTick` → `TickLifecycle` mapping, or the writer's
//! reconnect/buffer logic WOULD slip past source-scan ratchets.
//!
//! ## What this test does NOT cover (deliberate scope)
//!
//! - Live QuestDB ILP TCP write — the writer is `new_disconnected`
//!   which routes ticks to the ring buffer instead. We assert the
//!   ring buffer count increments correctly (proving the tick
//!   reached the persistence layer without panic).
//! - The `tick_processor.rs` SPSC consumer loop wiring — covered by
//!   `phase2_5_enricher_in_run_tick_processor.rs`.
//! - The exact ILP wire bytes — covered by
//!   `crates/storage/src/tick_persistence.rs::test_build_tick_row_enriched_writes_all_lifecycle_columns`.
//! - `now_ist_secs_of_day()` boundary jitter — `compute_phase` tests
//!   in `crates/common/src/phase.rs` cover every IST minute.

use tickvault_common::config::QuestDbConfig;
use tickvault_common::phase::Phase;
use tickvault_common::tick_types::ParsedTick;

use tickvault_core::pipeline::tick_enricher::TickEnricher;
use tickvault_storage::tick_persistence::{TickLifecycle, TickPersistenceWriter};

/// Build a writer in disconnected mode. `append_tick_enriched` will
/// route the row through `build_tick_row_enriched` and then attempt
/// reconnect; on failure (port 1 = unreachable) the tick is preserved
/// in the ring buffer so we can assert it reached persistence.
fn make_writer() -> TickPersistenceWriter {
    let cfg = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 1, // unreachable — disconnected mode
        pg_port: 1,
        ilp_port: 1,
    };
    TickPersistenceWriter::new_disconnected(&cfg)
}

/// Map an `EnrichedTick` to the persistence carrier — same one-liner
/// the production tick_processor uses.
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

/// Phase 3 prep e2e: a single OPEN tick flows through enricher →
/// TickLifecycle → append_tick_enriched. The writer rescues to ring
/// buffer (disconnected mode), proving the row was successfully
/// formatted by `build_tick_row_enriched` without panic before the
/// reconnect attempt.
#[test]
fn runtime_e2e_first_seen_open_tick_reaches_writer() {
    let enricher = TickEnricher::new();
    enricher.prev_oi_cache.insert(1234, 1, 75_000); // NSE_EQ
    let mut writer = make_writer();

    let tick = ParsedTick {
        security_id: 1234,
        exchange_segment_code: 1, // NSE_EQ
        last_traded_price: 250.5,
        volume: 12_500,
        day_close: 248.0,
        open_interest: 0,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 1_700_000_000_000_000_000,
        ..ParsedTick::default()
    };

    let now_ist_secs = 9 * 3600 + 30 * 60; // 09:30 IST = OPEN
    let enriched = enricher.enrich_tick(&tick, now_ist_secs);
    assert_eq!(enriched.phase, Phase::Open);
    assert!(enriched.volume_is_first_seen);
    assert_eq!(enriched.volume_delta, 12_500);
    assert_eq!(enriched.prev_day_close, 248.0);
    assert_eq!(enriched.prev_day_oi, 75_000);

    let life = lifecycle_from_enriched(&enriched);
    let result = writer.append_tick_enriched(&tick, life);
    assert!(
        result.is_ok(),
        "append_tick_enriched must succeed even when disconnected (rescue → ring buffer)"
    );

    // Row reached persistence — confirmed by ring buffer increment.
    assert_eq!(
        writer.buffered_tick_count(),
        1,
        "tick must rescue to ring buffer when disconnected (proves the format path ran)"
    );
}

/// Phase 3 prep e2e: PREMARKET tick flows through without panic +
/// reaches persistence. VOLUME-MONO-01 suppression in tick_processor.rs
/// uses the `volume_is_first_seen` + `phase != Open` flags from the
/// enricher; this test exercises the full path.
#[test]
fn runtime_e2e_premarket_tick_reaches_writer_and_phase_is_premarket() {
    let enricher = TickEnricher::new();
    let mut writer = make_writer();

    let tick = ParsedTick {
        security_id: 9999,
        exchange_segment_code: 1,
        last_traded_price: 100.0,
        volume: 500,
        day_close: 99.0,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 1_700_000_000_000_000_000,
        ..ParsedTick::default()
    };

    let now_ist_secs = 7 * 3600; // 07:00 IST = PREMARKET
    let enriched = enricher.enrich_tick(&tick, now_ist_secs);
    assert_eq!(enriched.phase, Phase::Premarket);
    assert!(enriched.volume_is_first_seen);

    let life = lifecycle_from_enriched(&enriched);
    let result = writer.append_tick_enriched(&tick, life);
    assert!(result.is_ok());
    assert_eq!(writer.buffered_tick_count(), 1);
}

/// Phase 3 prep e2e: midnight rollover end-to-end through the writer.
/// Day 1 OPEN tick stamps prev_day_close=100. Rollover clears state.
/// Day 2 PREMARKET tick stamps prev_day_close=105. Both ticks reach
/// the writer's ring buffer without panic.
#[test]
fn runtime_e2e_midnight_rollover_propagates_to_writer() {
    let enricher = TickEnricher::new();
    let mut writer = make_writer();

    let day1 = ParsedTick {
        security_id: 99,
        exchange_segment_code: 1,
        last_traded_price: 105.5,
        volume: 10_000,
        day_close: 100.0,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 1_700_000_000_000_000_000,
        ..ParsedTick::default()
    };
    let day1_enriched = enricher.enrich_tick(&day1, 9 * 3600 + 30 * 60);
    assert_eq!(day1_enriched.prev_day_close, 100.0);
    let _ = writer.append_tick_enriched(&day1, lifecycle_from_enriched(&day1_enriched));
    assert_eq!(writer.buffered_tick_count(), 1);

    enricher.rollover_for_new_day();

    let day2 = ParsedTick {
        security_id: 99,
        exchange_segment_code: 1,
        last_traded_price: 106.0,
        volume: 50,
        day_close: 105.0,
        exchange_timestamp: 1_700_086_400,
        received_at_nanos: 1_700_086_400_000_000_000,
        ..ParsedTick::default()
    };
    let day2_enriched = enricher.enrich_tick(&day2, 30 * 60);
    assert_eq!(
        day2_enriched.prev_day_close, 105.0,
        "rollover must clear stamper so day-2 stamps new prev_day_close"
    );
    assert!(day2_enriched.volume_is_first_seen);
    assert!(
        !day2_enriched.volume_is_regression,
        "L13 day-rollover invariant: post-clear volume reset is NOT a regression"
    );
    let _ = writer.append_tick_enriched(&day2, lifecycle_from_enriched(&day2_enriched));
    assert_eq!(
        writer.buffered_tick_count(),
        2,
        "both pre- and post-rollover ticks must reach persistence"
    );
}

/// Phase 3 prep e2e: I-P1-11 composite-key isolation. Two ticks with
/// the same `security_id` but different `exchange_segment_code`
/// produce DISTINCT lifecycle values from the enricher and both
/// reach the writer.
#[test]
fn runtime_e2e_composite_key_isolation_through_writer() {
    let enricher = TickEnricher::new();
    enricher.prev_oi_cache.insert(13, 0, 1_000_000); // NIFTY IDX_I
    enricher.prev_oi_cache.insert(13, 1, 50_000); // hypothetical NSE_EQ
    let mut writer = make_writer();

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
        exchange_timestamp: 1_700_000_001,
        received_at_nanos: 1_700_000_001_000_000_000,
        ..ParsedTick::default()
    };

    let r_idx = enricher.enrich_tick(&t_idx, now_ist_secs);
    let r_eq = enricher.enrich_tick(&t_eq, now_ist_secs);

    assert_eq!(r_idx.prev_day_oi, 1_000_000);
    assert_eq!(r_eq.prev_day_oi, 50_000);
    assert_eq!(r_idx.prev_day_close, 22500.0);
    assert_eq!(r_eq.prev_day_close, 1500.0);

    let _ = writer.append_tick_enriched(&t_idx, lifecycle_from_enriched(&r_idx));
    let _ = writer.append_tick_enriched(&t_eq, lifecycle_from_enriched(&r_eq));
    assert_eq!(
        writer.buffered_tick_count(),
        2,
        "both composite-key ticks must reach persistence independently"
    );
}

/// Phase 3 prep e2e: a sequence of 5 ticks for the SAME instrument
/// produces correctly incremental volume_deltas and all 5 reach the
/// writer.
#[test]
fn runtime_e2e_volume_delta_accumulates_across_5_ticks() {
    let enricher = TickEnricher::new();
    let mut writer = make_writer();

    let now_ist_secs = 9 * 3600 + 30 * 60;
    let cum_volumes: [u32; 5] = [1_000, 1_500, 1_500, 2_000, 2_750];
    let expected_deltas: [i64; 5] = [1_000, 500, 0, 500, 750];

    for (i, &v) in cum_volumes.iter().enumerate() {
        let tick = ParsedTick {
            security_id: 5050,
            exchange_segment_code: 1,
            volume: v,
            day_close: 200.0,
            exchange_timestamp: 1_700_000_000 + (i as u32),
            received_at_nanos: (1_700_000_000 + (i as i64)) * 1_000_000_000,
            ..ParsedTick::default()
        };
        let r = enricher.enrich_tick(&tick, now_ist_secs);
        assert_eq!(
            r.volume_delta, expected_deltas[i],
            "tick #{i}: cum={v}, expected delta={}",
            expected_deltas[i]
        );
        if i == 0 {
            assert!(r.volume_is_first_seen);
        } else {
            assert!(!r.volume_is_first_seen);
        }
        let _ = writer.append_tick_enriched(&tick, lifecycle_from_enriched(&r));
    }

    assert_eq!(
        writer.buffered_tick_count(),
        5,
        "all 5 ticks must reach persistence"
    );
}

/// Phase 3 prep e2e: panic safety. The enricher → writer chain MUST
/// NOT panic on a tick with garbage values (zero LTP, zero volume,
/// extreme timestamps). VOLUME-MONO-01 suppression depends on
/// EnrichedTick flags being correctly populated even on degenerate
/// inputs.
#[test]
fn runtime_e2e_panic_safety_on_garbage_tick() {
    let enricher = TickEnricher::new();
    let mut writer = make_writer();

    let garbage = ParsedTick {
        security_id: 0,
        exchange_segment_code: 255, // unknown segment
        last_traded_price: 0.0,
        volume: 0,
        day_close: 0.0,
        exchange_timestamp: 0,
        received_at_nanos: 0,
        ..ParsedTick::default()
    };

    let now_ist_secs = 9 * 3600 + 30 * 60;
    // MUST NOT panic.
    let enriched = enricher.enrich_tick(&garbage, now_ist_secs);
    let life = lifecycle_from_enriched(&enriched);
    let _ = writer.append_tick_enriched(&garbage, life);
    // We don't assert any specific outcome — just that no panic.
}
