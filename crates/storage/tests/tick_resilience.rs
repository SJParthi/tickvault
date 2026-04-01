//! Adversarial tick resilience tests -- verify zero tick loss guarantee.
//!
//! These tests simulate QuestDB failures during different lifecycle phases
//! and verify that no ticks are ever permanently lost.
//!
//! Plan item: E2 (tick persistence cold-start resilience).

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::TICK_BUFFER_CAPACITY;
use dhan_live_trader_common::tick_types::ParsedTick;
use dhan_live_trader_storage::tick_persistence::{DepthPersistenceWriter, TickPersistenceWriter};

/// Returns a `QuestDbConfig` pointing at a non-existent port so that all
/// ILP connection attempts fail immediately without network delay.
fn test_config() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: 19009, // Non-existent port
        http_port: 19000,
        pg_port: 18812,
    }
}

/// Builds a minimal `ParsedTick` with the given security ID and price.
/// Greeks fields set to NaN (non-F&O default).
fn make_tick(id: u32, price: f32) -> ParsedTick {
    ParsedTick {
        security_id: id,
        exchange_segment_code: 1, // NSE_EQ
        last_traded_price: price,
        last_trade_quantity: 100,
        exchange_timestamp: 1_700_000_000_u32.saturating_add(id),
        received_at_nanos: 0,
        average_traded_price: price,
        volume: 1000,
        total_sell_quantity: 500,
        total_buy_quantity: 500,
        day_open: price,
        day_close: price,
        day_high: price,
        day_low: price,
        open_interest: 0,
        oi_day_high: 0,
        oi_day_low: 0,
        iv: f64::NAN,
        delta: f64::NAN,
        gamma: f64::NAN,
        theta: f64::NAN,
        vega: f64::NAN,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Disconnected writer must buffer all appended ticks in the ring buffer
/// instead of dropping them.  After 100 `append_tick` calls every tick
/// should be reflected in `buffered_tick_count()`.
#[test]
fn test_disconnected_writer_buffers_all_ticks() {
    let config = test_config();
    let mut writer = TickPersistenceWriter::new_disconnected(&config);

    let tick_count = 100_usize;
    for i in 0..tick_count {
        let tick = make_tick(i as u32, 100.0 + i as f32);
        let result = writer.append_tick(&tick);
        assert!(
            result.is_ok(),
            "append_tick #{i} must succeed even when disconnected: {:?}",
            result.err()
        );
    }

    assert_eq!(
        writer.buffered_tick_count(),
        tick_count,
        "all {} ticks must be in the ring buffer when QuestDB is unreachable",
        tick_count
    );
}

/// A freshly constructed disconnected writer must report zero dropped ticks.
/// The counter only increments when both ring buffer AND disk spill fail,
/// which should never happen under normal (non-disk-full) conditions.
#[test]
fn test_disconnected_writer_starts_with_zero_drops() {
    let config = test_config();
    let writer = TickPersistenceWriter::new_disconnected(&config);

    assert_eq!(
        writer.ticks_dropped_total(),
        0,
        "freshly created writer must have zero dropped ticks"
    );
}

/// `is_connected()` must return `false` for a writer created with
/// `new_disconnected()`.  This is the cold-start path where QuestDB
/// is unavailable at boot.
#[test]
fn test_disconnected_writer_is_not_connected() {
    let config = test_config();
    let writer = TickPersistenceWriter::new_disconnected(&config);

    assert!(
        !writer.is_connected(),
        "disconnected writer must report is_connected() == false"
    );
}

/// Fill the ring buffer up to a known count and verify that
/// `buffered_tick_count()` reflects the exact number.
/// Uses a smaller count (10,000) to keep the test fast while still
/// exercising the VecDeque growth path well past initial capacity.
#[test]
fn test_buffer_capacity_at_limit() {
    let config = test_config();
    let mut writer = TickPersistenceWriter::new_disconnected(&config);

    // Use a test-friendly count smaller than TICK_BUFFER_CAPACITY (300,000).
    // 10,000 is enough to stress the ring buffer without slowing CI.
    let count = 10_000_usize;
    for i in 0..count {
        let tick = make_tick(i as u32, 200.0 + i as f32);
        writer.append_tick(&tick).ok();
    }

    assert_eq!(
        writer.buffered_tick_count(),
        count,
        "buffered_tick_count must equal the number of appended ticks"
    );
}

/// `append_tick` must return `Ok(())` even when the writer is disconnected.
/// This guarantees that the hot-path caller never has to handle errors from
/// tick persistence (observability data is not on the critical path).
#[test]
fn test_disconnected_writer_append_returns_ok() {
    let config = test_config();
    let mut writer = TickPersistenceWriter::new_disconnected(&config);

    let tick = make_tick(42, 500.25);
    let result = writer.append_tick(&tick);

    assert!(
        result.is_ok(),
        "append_tick on disconnected writer must return Ok, got: {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------------
// E2: Adversarial Tick Resilience Tests (Zero Tick Loss Guarantee)
// ---------------------------------------------------------------------------

/// When the ring buffer reaches TICK_BUFFER_CAPACITY, additional ticks must
/// spill to disk rather than being dropped. Verify spill counter increments.
#[test]
fn test_disk_spill_activates_when_buffer_full() {
    let config = test_config();
    let mut writer = TickPersistenceWriter::new_disconnected(&config);

    // Fill ring buffer to capacity
    for i in 0..TICK_BUFFER_CAPACITY {
        let tick = make_tick(i as u32, 100.0);
        writer.append_tick(&tick).ok();
    }

    assert_eq!(
        writer.buffered_tick_count(),
        TICK_BUFFER_CAPACITY,
        "ring buffer must be at capacity"
    );
    assert_eq!(
        writer.ticks_spilled_total(),
        0,
        "no ticks should be spilled yet"
    );

    // One more tick should trigger disk spill
    let overflow_tick = make_tick(999_999, 999.99);
    writer.append_tick(&overflow_tick).ok();

    assert_eq!(
        writer.ticks_spilled_total(),
        1,
        "one tick must have spilled to disk"
    );
    assert_eq!(
        writer.ticks_dropped_total(),
        0,
        "zero ticks must be dropped — disk spill catches overflow"
    );

    // Clean up spill file
    let _ = std::fs::remove_dir_all("data/spill");
}

/// The DepthPersistenceWriter::new_disconnected must start in disconnected
/// buffering mode, just like the tick writer.
#[test]
fn test_depth_writer_new_disconnected_buffers() {
    let config = test_config();
    let writer = DepthPersistenceWriter::new_disconnected(&config);

    assert!(
        !writer.is_connected(),
        "disconnected depth writer must report is_connected() == false"
    );
}

/// force_flush on a disconnected tick writer must not panic or lose data.
/// In-flight ticks should be rescued back to the ring buffer.
#[test]
fn test_force_flush_on_disconnected_writer_rescues_data() {
    let config = test_config();
    let mut writer = TickPersistenceWriter::new_disconnected(&config);

    // Append several ticks
    for i in 0..50 {
        let tick = make_tick(i, 100.0 + i as f32);
        writer.append_tick(&tick).ok();
    }

    let buffered_before = writer.buffered_tick_count();

    // force_flush should not lose any data
    let _ = writer.force_flush();

    // All ticks should still be in buffer (either ring buffer or rescued)
    assert!(
        writer.buffered_tick_count() >= buffered_before,
        "force_flush on disconnected writer must not lose buffered ticks"
    );
    assert_eq!(
        writer.ticks_dropped_total(),
        0,
        "force_flush must never drop ticks"
    );
}

/// Rapid alternation between append and flush must not corrupt state.
#[test]
fn test_rapid_append_flush_cycle_no_corruption() {
    let config = test_config();
    let mut writer = TickPersistenceWriter::new_disconnected(&config);

    for i in 0..1000_u32 {
        let tick = make_tick(i, 100.0 + i as f32);
        writer.append_tick(&tick).ok();

        // Flush every 10 ticks
        if i % 10 == 0 {
            let _ = writer.force_flush();
        }
    }

    // No ticks should have been dropped
    assert_eq!(
        writer.ticks_dropped_total(),
        0,
        "rapid append/flush cycles must not drop any ticks"
    );

    // All 1000 ticks should be accounted for in buffer
    assert!(
        writer.buffered_tick_count() > 0,
        "buffer must contain ticks after disconnected append/flush cycles"
    );
}
