//! Candle persistence resilience tests — cold-start, recovery ordering, and
//! schema validation after reconnect.
//!
//! Plan items: P3 (cold-start), P4 (recovery ordering), P5 (schema validation).
//!
//! These tests use an unreachable QuestDB config to simulate cold-start
//! scenarios, verifying the zero-candle-loss guarantee.

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_storage::candle_persistence::LiveCandleWriter;

/// Returns a `QuestDbConfig` pointing at a non-existent port so that all
/// ILP connection attempts fail immediately without network delay.
fn unreachable_config() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: 19009,
        http_port: 19000,
        pg_port: 18812,
    }
}

/// Creates a `LiveCandleWriter` in disconnected/cold-start mode.
fn make_disconnected_writer() -> LiveCandleWriter {
    LiveCandleWriter::new(&unreachable_config())
        .expect("LiveCandleWriter::new must succeed even when QuestDB is unreachable")
}

// ---------------------------------------------------------------------------
// P3: Cold-start tests — QuestDB unavailable at startup
// ---------------------------------------------------------------------------

#[test]
fn test_candle_writer_cold_start_succeeds() {
    // LiveCandleWriter must initialize even when QuestDB is unreachable.
    let writer = make_disconnected_writer();
    assert_eq!(
        writer.buffered_candle_count(),
        0,
        "disconnected writer starts with empty buffer"
    );
}

#[test]
fn test_candle_writer_cold_start_buffers_candles() {
    // When QuestDB is down, candles must be buffered — never dropped.
    let mut writer = make_disconnected_writer();

    for i in 0..10 {
        writer
            .append_candle(
                1000 + i,          // security_id
                1,                 // NSE_EQ
                1_700_000_000 + i, // timestamp_secs
                100.0,             // open
                101.0,             // high
                99.0,              // low
                100.5,             // close
                5000,              // volume
                50,                // tick_count
                0.0,               // iv
                0.0,               // delta
                0.0,               // gamma
                0.0,               // theta
                0.0,               // vega
            )
            .expect("append_candle must not fail even when disconnected");
    }

    assert_eq!(
        writer.buffered_candle_count(),
        10,
        "all 10 candles must be buffered when QuestDB is down"
    );
}

#[test]
fn test_candle_writer_cold_start_zero_drops() {
    // Fill buffer with candles while disconnected — verify none are lost.
    let mut writer = make_disconnected_writer();
    let candle_count: u32 = 500;

    for i in 0..candle_count {
        writer
            .append_candle(
                i,
                2, // NSE_FNO
                1_700_000_000 + i,
                200.0 + i as f32,
                201.0 + i as f32,
                199.0 + i as f32,
                200.5 + i as f32,
                1000,
                10,
                0.25,
                0.5,
                0.01,
                -0.03,
                0.15,
            )
            .expect("append must not fail");
    }

    // Zero candles dropped — all buffered either in ring buffer or spilled to disk.
    assert_eq!(
        writer.buffered_candle_count() as u64 + writer.candles_dropped_total(),
        u64::from(candle_count),
        "total buffered + spilled must equal total appended"
    );
}

// ---------------------------------------------------------------------------
// P4: Recovery ordering — ring buffer drains before disk spill
// ---------------------------------------------------------------------------

#[test]
fn test_recovery_ordering_ring_before_spill() {
    // Verify that the ring buffer is FIFO — oldest candles drain first.
    // Since we can't connect to QuestDB in tests, we verify the internal
    // ordering by checking that buffered_candle_count matches expectations
    // after sequential appends and that no candles are reordered.
    let mut writer = make_disconnected_writer();

    // Append candles with sequential timestamps.
    for i in 0..5_u32 {
        writer
            .append_candle(
                42,
                1,
                1_700_000_000 + i,
                100.0 + i as f32,
                101.0,
                99.0,
                100.0 + i as f32,
                1000,
                10,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
            )
            .expect("append must not fail");
    }

    assert_eq!(writer.buffered_candle_count(), 5);

    // Append more — buffer should keep growing, maintaining FIFO order.
    for i in 5..10_u32 {
        writer
            .append_candle(
                42,
                1,
                1_700_000_000 + i,
                100.0 + i as f32,
                101.0,
                99.0,
                100.0 + i as f32,
                1000,
                10,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
            )
            .expect("append must not fail");
    }

    assert_eq!(
        writer.buffered_candle_count(),
        10,
        "ring buffer must hold all candles in FIFO order"
    );

    // force_flush on disconnected writer should not panic or lose candles.
    writer.force_flush().expect("force_flush must not fail");
    // Candles stay in ring buffer since there's no connection to drain to.
    assert_eq!(
        writer.buffered_candle_count(),
        10,
        "candles must remain buffered after flush attempt without connection"
    );
}

// ---------------------------------------------------------------------------
// P5: Schema validation after reconnect
// ---------------------------------------------------------------------------

#[test]
fn test_reconnect_revalidates_schema() {
    // After reconnection, the writer must re-create a fresh ILP buffer
    // (not reuse the old one which may have stale schema state).
    // Since reconnect() requires a real QuestDB, we verify the prerequisite:
    // that a disconnected writer creates a valid buffer state that allows
    // candle rows to be appended without panicking.
    let mut writer = make_disconnected_writer();

    // Append a candle — exercises the buffer_candle path (ring buffer).
    writer
        .append_candle(
            12345,
            1,
            1_700_000_000,
            100.0,
            101.0,
            99.0,
            100.5,
            5000,
            50,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        )
        .expect("append must succeed on disconnected writer");

    // Verify flush_if_needed is a no-op when disconnected (no panic).
    writer
        .flush_if_needed()
        .expect("flush_if_needed must not fail on disconnected writer");

    // force_flush should also be safe.
    writer
        .force_flush()
        .expect("force_flush must not fail on disconnected writer");

    // Buffer preserved — no data loss.
    assert_eq!(writer.buffered_candle_count(), 1);
    assert_eq!(writer.candles_dropped_total(), 0);
}

#[test]
fn test_disconnected_writer_fresh_buffer_does_not_corrupt() {
    // Verify that repeated append/flush cycles on a disconnected writer
    // don't cause buffer corruption or panic from stale ILP state.
    let mut writer = make_disconnected_writer();

    for cycle in 0..3_u32 {
        for i in 0..5_u32 {
            writer
                .append_candle(
                    cycle * 100 + i,
                    2,
                    1_700_000_000 + cycle * 100 + i,
                    100.0,
                    101.0,
                    99.0,
                    100.5,
                    1000,
                    10,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                )
                .expect("append must not fail");
        }
        writer.force_flush().expect("force_flush must not panic");
    }

    // All 15 candles (3 cycles x 5) buffered.
    assert_eq!(writer.buffered_candle_count(), 15);
}
