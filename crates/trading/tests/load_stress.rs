//! Load and stress tests for trading crate components.
//!
//! Verifies risk engine and indicator ring buffer handle
//! sustained throughput without degradation.

use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Sustained: Risk engine under load
// ---------------------------------------------------------------------------

#[test]
fn stress_risk_engine_100k_checks() {
    use dhan_live_trader_trading::risk::engine::RiskEngine;

    let mut engine = RiskEngine::new(2.0, 1000, 10_000_000.0);
    let start = Instant::now();

    for i in 0..100_000_u32 {
        let security_id = 50000 + (i % 1000);
        let lots = ((i % 10) as i32) + 1;
        let _ = engine.check_order(security_id, lots);

        // Simulate some fills to build position state
        if i % 100 == 0 {
            engine.record_fill(security_id, lots, 100.0 + (i as f64 * 0.01), 25);
        }
    }

    let elapsed = start.elapsed();
    assert_eq!(engine.total_checks(), 100_000);
    assert!(
        elapsed < Duration::from_secs(1),
        "100K risk checks took {elapsed:?} — too slow"
    );
}

// ---------------------------------------------------------------------------
// Sustained: Indicator ring buffer under load
// ---------------------------------------------------------------------------

#[test]
fn stress_ring_buffer_1m_pushes() {
    use dhan_live_trader_trading::indicator::types::RingBuffer;

    let mut ring = RingBuffer::new();
    let start = Instant::now();

    for i in 0..1_000_000_u64 {
        let _ = ring.push(i as f64 * 0.01);
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "1M ring buffer pushes took {elapsed:?} — too slow"
    );
    // Capacity should be stable
    assert_eq!(
        ring.len() as usize,
        dhan_live_trader_common::constants::INDICATOR_RING_BUFFER_CAPACITY
    );
}

// ---------------------------------------------------------------------------
// Memory stability: repeated operations shouldn't grow memory
// ---------------------------------------------------------------------------

#[test]
fn stress_no_unbounded_growth_risk_engine() {
    use dhan_live_trader_trading::risk::engine::RiskEngine;

    let mut engine = RiskEngine::new(2.0, 10000, 100_000_000.0);

    // Open and close positions repeatedly on same instruments
    for round in 0..100_u32 {
        for sec in 0..100_u32 {
            engine.record_fill(sec, 10, 100.0 + round as f64, 25);
            engine.record_fill(sec, -10, 101.0 + round as f64, 25);
        }
    }

    // Position count should be bounded (all positions closed)
    let open = engine.open_position_count();
    assert_eq!(open, 0, "all positions should be closed, got {open} open");
}
