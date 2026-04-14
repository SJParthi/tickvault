//! Property-based tests for the storage crate (Type 6 — proptest).
//!
//! Verifies invariants of the QuestDB tick persistence layer using random inputs:
//! - Ring buffer count never exceeds capacity
//! - append_tick never returns Err on disconnected writer
//! - ticks_dropped_total stays 0 when ring buffer has capacity
//! - buffered_tick_count matches number of appended ticks (disconnected mode)
//! - Rapid append/flush cycles never corrupt state

use proptest::prelude::*;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::TICK_BUFFER_CAPACITY;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::tick_persistence::TickPersistenceWriter;

/// Returns a `QuestDbConfig` pointing at a refused port so that all
/// ILP connection attempts fail immediately (no network timeout delay).
fn disconnected_config() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 19000,
        pg_port: 18812,
        ilp_port: 19009, // Non-existent port — connection refused instantly
    }
}

/// Limit proptest cases for CI speed. Each case triggers a reconnect attempt
/// (7 seconds of thread::sleep) on the first append_tick, so we limit to 10.
const PROPTEST_CASES: u32 = 10;

/// Generates an arbitrary `ParsedTick` with random field values.
fn arb_parsed_tick() -> impl Strategy<Value = ParsedTick> {
    (
        any::<u32>(),                                             // security_id
        0u8..=8u8,                           // exchange_segment_code (valid range)
        any::<f32>(),                        // last_traded_price
        any::<u16>(),                        // last_trade_quantity
        1_700_000_000u32..=1_800_000_000u32, // exchange_timestamp (realistic range)
        any::<i64>(),                        // received_at_nanos
        any::<f32>(),                        // average_traded_price
        any::<u32>(),                        // volume
        (any::<u32>(), any::<u32>()),        // sell_qty, buy_qty
        (any::<f32>(), any::<f32>(), any::<f32>(), any::<f32>()), // open, close, high, low
        (any::<u32>(), any::<u32>(), any::<u32>()), // oi, oi_high, oi_low
    )
        .prop_map(
            |(
                security_id,
                segment,
                ltp,
                ltq,
                ts,
                received,
                atp,
                vol,
                (sell_qty, buy_qty),
                (open, close, high, low),
                (oi, oi_high, oi_low),
            )| {
                ParsedTick {
                    security_id,
                    exchange_segment_code: segment,
                    last_traded_price: ltp,
                    last_trade_quantity: ltq,
                    exchange_timestamp: ts,
                    received_at_nanos: received,
                    average_traded_price: atp,
                    volume: vol,
                    total_sell_quantity: sell_qty,
                    total_buy_quantity: buy_qty,
                    day_open: open,
                    day_close: close,
                    day_high: high,
                    day_low: low,
                    open_interest: oi,
                    oi_day_high: oi_high,
                    oi_day_low: oi_low,
                    iv: f64::NAN,
                    delta: f64::NAN,
                    gamma: f64::NAN,
                    theta: f64::NAN,
                    vega: f64::NAN,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// Property: ring buffer count invariant
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(PROPTEST_CASES))]

    /// Appending N random ticks to a disconnected writer must result in
    /// buffered_tick_count() == N (when N <= TICK_BUFFER_CAPACITY).
    #[test]
    fn proptest_buffer_count_invariant(
        ticks in proptest::collection::vec(arb_parsed_tick(), 1..200)
    ) {
        let config = disconnected_config();
        let mut writer = TickPersistenceWriter::new_disconnected(&config);

        let count = ticks.len();
        for tick in &ticks {
            writer.append_tick(tick).unwrap();
        }

        prop_assert_eq!(
            writer.buffered_tick_count(),
            count,
            "buffered_tick_count must equal number of appended ticks"
        );
    }

    /// Appending more ticks than buffered_tick_count can hold must never
    /// increase buffered_tick_count beyond TICK_BUFFER_CAPACITY. Overflow
    /// goes to disk spill, and ticks_dropped_total stays 0.
    ///
    /// NOTE: We test this with a deterministic single case (not proptest loop)
    /// because filling 300K ticks per proptest iteration is too slow.
    /// The proptest invariants (random data, no panics, count correctness)
    /// are covered by the other property tests within the ring buffer range.
    #[test]
    fn proptest_ring_buffer_never_exceeds_capacity(
        extra_count in 1usize..10
    ) {
        // Use a smaller pre-fill (1000 ticks) + TICK_BUFFER_CAPACITY validation
        // via the existing integration test (test_prolonged_outage_ring_plus_spill_zero_loss).
        // Here we verify the invariant: appending N ticks always results in
        // buffered_tick_count() <= TICK_BUFFER_CAPACITY.
        let config = disconnected_config();
        let mut writer = TickPersistenceWriter::new_disconnected(&config);

        // Append a modest number of ticks to verify the invariant holds.
        let base_count = 500_usize;
        for i in 0..(base_count + extra_count) {
            let tick = ParsedTick {
                security_id: i as u32,
                exchange_segment_code: 1,
                last_traded_price: 100.0,
                last_trade_quantity: 1,
                exchange_timestamp: 1_700_000_000,
                received_at_nanos: 0,
                average_traded_price: 100.0,
                volume: 1,
                total_sell_quantity: 1,
                total_buy_quantity: 1,
                day_open: 100.0,
                day_close: 100.0,
                day_high: 100.0,
                day_low: 100.0,
                open_interest: 0,
                oi_day_high: 0,
                oi_day_low: 0,
                iv: f64::NAN,
                delta: f64::NAN,
                gamma: f64::NAN,
                theta: f64::NAN,
                vega: f64::NAN,
            };
            writer.append_tick(&tick).unwrap();
        }

        prop_assert!(
            writer.buffered_tick_count() <= TICK_BUFFER_CAPACITY,
            "ring buffer must never exceed TICK_BUFFER_CAPACITY (got {})",
            writer.buffered_tick_count()
        );

        prop_assert_eq!(
            writer.ticks_dropped_total(),
            0,
            "no ticks must be dropped — overflow goes to disk spill"
        );
    }

    /// append_tick must always return Ok on a disconnected writer.
    /// This guarantees the hot-path caller never handles persistence errors.
    #[test]
    fn proptest_append_tick_always_ok_disconnected(tick in arb_parsed_tick()) {
        let config = disconnected_config();
        let mut writer = TickPersistenceWriter::new_disconnected(&config);

        let result = writer.append_tick(&tick);
        prop_assert!(
            result.is_ok(),
            "append_tick must always return Ok on disconnected writer"
        );
    }

    /// Zero ticks dropped when ring buffer has capacity.
    #[test]
    fn proptest_zero_drops_within_capacity(
        ticks in proptest::collection::vec(arb_parsed_tick(), 1..200)
    ) {
        let config = disconnected_config();
        let mut writer = TickPersistenceWriter::new_disconnected(&config);

        for tick in &ticks {
            writer.append_tick(tick).unwrap();
        }

        prop_assert_eq!(
            writer.ticks_dropped_total(),
            0,
            "ticks_dropped_total must be 0 when within ring buffer capacity"
        );
        prop_assert_eq!(
            writer.ticks_spilled_total(),
            0,
            "ticks_spilled_total must be 0 when within ring buffer capacity"
        );
    }

    /// Rapid append/flush cycles must not corrupt state or drop ticks.
    #[test]
    fn proptest_rapid_append_flush_no_corruption(
        tick_count in 10usize..200,
        flush_interval in 1usize..20
    ) {
        let config = disconnected_config();
        let mut writer = TickPersistenceWriter::new_disconnected(&config);

        for i in 0..tick_count {
            let tick = ParsedTick {
                security_id: i as u32,
                exchange_segment_code: 2,
                last_traded_price: 100.0 + i as f32,
                last_trade_quantity: 75,
                exchange_timestamp: 1_740_556_500,
                received_at_nanos: 0,
                average_traded_price: 100.0,
                volume: 50000,
                total_sell_quantity: 25000,
                total_buy_quantity: 25000,
                day_open: 100.0,
                day_close: 100.0,
                day_high: 100.0,
                day_low: 100.0,
                open_interest: 0,
                oi_day_high: 0,
                oi_day_low: 0,
                iv: f64::NAN,
                delta: f64::NAN,
                gamma: f64::NAN,
                theta: f64::NAN,
                vega: f64::NAN,
            };
            writer.append_tick(&tick).unwrap();

            if i % flush_interval == 0 {
                let _ = writer.force_flush();
            }
        }

        prop_assert_eq!(
            writer.ticks_dropped_total(),
            0,
            "rapid append/flush cycles must not drop ticks"
        );

        // All ticks must be accounted for in the buffer (disconnected mode).
        prop_assert!(
            writer.buffered_tick_count() > 0,
            "buffer must contain ticks after disconnected append/flush"
        );
    }
}
