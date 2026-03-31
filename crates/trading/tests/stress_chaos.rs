//! Stress and chaos tests for trading crate components.
//!
//! Validates that OMS, risk engine, indicator engine, and state machine
//! handle extreme loads, rapid state changes, and adversarial inputs
//! without panics, data corruption, or incorrect behavior.
//!
//! NOTE: Timing assertions are skipped under instrumented builds
//! (cargo-careful, sanitizers). Set `DLT_SKIP_PERF_ASSERTIONS=1` to skip.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use std::time::{Duration, Instant};

/// Returns true when running under instrumented builds (cargo-careful, sanitizers).
fn skip_perf_assertions() -> bool {
    std::env::var("DLT_SKIP_PERF_ASSERTIONS").is_ok()
}

// ===========================================================================
// 1. Rate Limiter — stress tests
// ===========================================================================

mod stress_rate_limiter {
    use dhan_live_trader_trading::oms::rate_limiter::OrderRateLimiter;

    #[test]
    fn test_stress_rate_limiter_10k_rapid_submissions() {
        // Submit 10,000 orders rapidly — rate limiter must block all beyond burst.
        let limiter = OrderRateLimiter::new(10);
        let mut allowed = 0_u32;
        let mut denied = 0_u32;

        for _ in 0..10_000 {
            match limiter.check() {
                Ok(()) => allowed += 1,
                Err(_) => denied += 1,
            }
        }

        // SEBI limit is 10/sec burst. First 10 allowed, rest denied.
        assert_eq!(allowed, 10, "exactly 10 orders must be allowed in burst");
        assert_eq!(denied, 9990, "9990 orders must be denied after burst");
    }

    #[test]
    fn test_stress_rate_limiter_burst_exactly_at_limit() {
        let limiter = OrderRateLimiter::new(10);
        for i in 0..10 {
            assert!(
                limiter.check().is_ok(),
                "order {i} within burst must be allowed"
            );
        }
        assert!(
            limiter.check().is_err(),
            "order 11 must be denied (burst exhausted)"
        );
    }

    #[test]
    fn test_stress_rate_limiter_single_order_limit() {
        let limiter = OrderRateLimiter::new(1);
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_err());
        assert!(limiter.check().is_err());
    }

    #[test]
    fn test_stress_rate_limiter_high_burst_all_pass() {
        // With burst = 1000, all 1000 should pass instantly.
        let limiter = OrderRateLimiter::new(1000);
        for i in 0..1000 {
            assert!(
                limiter.check().is_ok(),
                "order {i} within high burst must pass"
            );
        }
        assert!(limiter.check().is_err(), "order 1001 must be denied");
    }

    #[test]
    fn test_stress_rate_limiter_deny_count_matches() {
        let limiter = OrderRateLimiter::new(5);
        let mut denied = 0_u32;
        for _ in 0..100 {
            if limiter.check().is_err() {
                denied += 1;
            }
        }
        assert_eq!(denied, 95);
    }
}

// ===========================================================================
// 2. Circuit Breaker — chaos tests
// ===========================================================================

mod chaos_circuit_breaker {
    use super::*;
    use dhan_live_trader_common::constants::OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD;
    use dhan_live_trader_trading::oms::circuit_breaker::{CircuitState, OrderCircuitBreaker};
    use std::sync::Arc;

    #[test]
    fn test_chaos_circuit_breaker_rapid_failure_success_alternation() {
        let cb = OrderCircuitBreaker::new();

        // Rapidly alternate between failure and success 1000 times.
        // Circuit breaker must never panic and must stay consistent.
        for i in 0..1000_u32 {
            if i % 2 == 0 {
                cb.record_failure();
            } else {
                cb.record_success();
            }
            // State must always be one of the 3 valid states.
            let state = cb.state();
            assert!(
                state == CircuitState::Closed
                    || state == CircuitState::Open
                    || state == CircuitState::HalfOpen,
                "invalid state after iteration {i}"
            );
        }
    }

    #[test]
    fn test_chaos_circuit_breaker_trip_and_recover_cycle() {
        let cb = OrderCircuitBreaker::new();

        for cycle in 0..50 {
            // Trip the circuit breaker
            for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
                cb.record_failure();
            }
            assert_ne!(
                cb.state(),
                CircuitState::Closed,
                "must not be closed after {OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD} failures (cycle {cycle})"
            );

            // Reset to closed
            cb.reset();
            assert_eq!(
                cb.state(),
                CircuitState::Closed,
                "must be closed after reset (cycle {cycle})"
            );
            assert!(
                cb.check().is_ok(),
                "must allow requests after reset (cycle {cycle})"
            );
        }
    }

    #[test]
    fn test_chaos_circuit_breaker_concurrent_failures_from_threads() {
        let cb = Arc::new(OrderCircuitBreaker::new());
        let mut handles = vec![];

        for _ in 0..100 {
            let cb_clone = Arc::clone(&cb);
            handles.push(std::thread::spawn(move || {
                cb_clone.record_failure();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // After 100 failures, circuit must not be Closed (threshold is 3).
        assert_ne!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_chaos_circuit_breaker_concurrent_mixed_operations() {
        let cb = Arc::new(OrderCircuitBreaker::new());
        let mut handles = vec![];

        for i in 0..200 {
            let cb_clone = Arc::clone(&cb);
            handles.push(std::thread::spawn(move || match i % 4 {
                0 => cb_clone.record_failure(),
                1 => cb_clone.record_success(),
                2 => {
                    let _ = cb_clone.check();
                }
                3 => cb_clone.reset(),
                _ => unreachable!(),
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Must not panic. Final state is valid.
        let state = cb.state();
        assert!(
            state == CircuitState::Closed
                || state == CircuitState::Open
                || state == CircuitState::HalfOpen,
        );
    }

    #[test]
    fn test_chaos_circuit_breaker_exact_threshold_boundary() {
        let cb = OrderCircuitBreaker::new();

        // One below threshold: still closed
        for _ in 0..(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD - 1) {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Closed);

        // One more failure: trips the breaker
        cb.record_failure();
        assert_ne!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_stress_circuit_breaker_100k_checks_when_closed() {
        let cb = OrderCircuitBreaker::new();
        let start = Instant::now();

        for _ in 0..100_000 {
            assert!(cb.check().is_ok());
        }

        let elapsed = start.elapsed();
        if !skip_perf_assertions() {
            assert!(
                elapsed < Duration::from_millis(500),
                "100K circuit breaker checks took {elapsed:?}"
            );
        }
    }
}

// ===========================================================================
// 3. Risk Engine — concurrent fills and P&L stress
// ===========================================================================

mod stress_risk_engine {
    use super::*;
    use dhan_live_trader_trading::risk::engine::RiskEngine;
    use dhan_live_trader_trading::risk::types::RiskBreach;

    #[test]
    fn test_stress_risk_engine_multiple_fills_same_security() {
        let mut engine = RiskEngine::new(2.0, 1000, 10_000_000.0);

        // Buy 100 lots, then sell 100 lots — P&L must settle correctly.
        let security_id = 50000;
        for _ in 0..100 {
            engine.record_fill(security_id, 1, 100.0, 25);
        }
        for _ in 0..100 {
            engine.record_fill(security_id, -1, 110.0, 25);
        }

        // 100 lots * 10 rupee profit * 25 lot_size = 25,000 realized P&L.
        let realized = engine.total_realized_pnl();
        assert!(
            (realized - 25_000.0).abs() < 1.0,
            "realized P&L should be ~25,000, got {realized}"
        );
    }

    #[test]
    fn test_stress_risk_engine_position_overflow_protection() {
        let mut engine = RiskEngine::new(2.0, 10, 10_000_000.0);

        // Try to exceed position limit.
        let check = engine.check_order(50000, 11);
        assert!(!check.is_approved(), "11 lots must be rejected (max is 10)");
    }

    #[test]
    fn test_stress_risk_engine_daily_loss_halt() {
        // Capital = 100,000. Max loss = 2% = 2,000.
        let mut engine = RiskEngine::new(2.0, 1000, 100_000.0);

        // Buy 500 lots at 100, market drops to 80.
        // Unrealized = 500 * (80 - 100) = -10,000.
        // -10,000 > 2,000 threshold.
        engine.record_fill(50000, 500, 100.0, 25);
        engine.update_market_price(50000, 80.0);

        let check = engine.check_order(50001, 1);
        assert!(!check.is_approved(), "must halt after daily loss exceeded");
        assert!(engine.is_halted(), "engine must be halted");
    }

    #[test]
    fn test_stress_risk_engine_halt_blocks_all_subsequent() {
        let mut engine = RiskEngine::new(2.0, 1000, 1_000_000.0);
        engine.manual_halt();

        // Every subsequent order must be rejected.
        for i in 0..1000_u32 {
            let check = engine.check_order(50000 + i, 1);
            assert!(
                !check.is_approved(),
                "order {i} must be rejected after halt"
            );
        }
    }

    #[test]
    fn test_stress_risk_engine_daily_reset_clears_everything() {
        let mut engine = RiskEngine::new(2.0, 1000, 10_000_000.0);

        engine.record_fill(50000, 10, 100.0, 25);
        engine.update_market_price(50000, 200.0);
        engine.manual_halt();

        engine.reset_daily();

        assert!(!engine.is_halted());
        assert_eq!(engine.total_realized_pnl(), 0.0);
        assert_eq!(engine.total_checks(), 0);
        assert_eq!(engine.total_rejections(), 0);

        // Should accept orders again.
        let check = engine.check_order(50000, 1);
        assert!(check.is_approved());
    }

    #[test]
    fn test_stress_risk_engine_100k_mixed_operations() {
        let mut engine = RiskEngine::new(5.0, 1000, 100_000_000.0);
        let start = Instant::now();

        for i in 0..100_000_u32 {
            let security_id = 50000 + (i % 500);
            let lots = ((i % 5) as i32) + 1;

            let _ = engine.check_order(security_id, lots);

            if i % 50 == 0 {
                engine.record_fill(security_id, lots, 100.0 + (i as f64 * 0.001), 25);
            }
            if i % 100 == 0 {
                engine.update_market_price(security_id, 100.0 + (i as f64 * 0.002));
            }
        }

        let elapsed = start.elapsed();
        assert_eq!(engine.total_checks(), 100_000);
        if !skip_perf_assertions() {
            assert!(
                elapsed < Duration::from_secs(2),
                "100K mixed risk ops took {elapsed:?}"
            );
        }
    }

    #[test]
    fn test_stress_risk_engine_pnl_never_nan() {
        let mut engine = RiskEngine::new(2.0, 1000, 10_000_000.0);

        // Feed a wide variety of prices including edge values.
        let prices = [
            0.01,
            0.1,
            1.0,
            10.0,
            100.0,
            1000.0,
            10000.0,
            99999.99,
            f64::MAX / 2.0,
        ];

        for (i, &price) in prices.iter().enumerate() {
            let sid = 50000 + (i as u32);
            engine.record_fill(sid, 1, price, 25);
            engine.update_market_price(sid, price * 1.1);
        }

        let realized = engine.total_realized_pnl();
        let unrealized = engine.total_unrealized_pnl();
        assert!(realized.is_finite(), "realized P&L must be finite");
        assert!(unrealized.is_finite(), "unrealized P&L must be finite");
    }

    #[test]
    fn test_stress_risk_engine_update_market_price_rejects_invalid() {
        let mut engine = RiskEngine::new(2.0, 1000, 10_000_000.0);

        // These should be silently rejected (non-positive and non-finite).
        engine.update_market_price(50000, 0.0);
        engine.update_market_price(50000, -100.0);
        engine.update_market_price(50000, f64::NAN);
        engine.update_market_price(50000, f64::INFINITY);
        engine.update_market_price(50000, f64::NEG_INFINITY);

        // No position set, so unrealized P&L should be 0.
        assert_eq!(engine.total_unrealized_pnl(), 0.0);
    }

    #[test]
    fn test_stress_risk_engine_halt_reason_tracking() {
        let mut engine = RiskEngine::new(2.0, 1000, 10_000_000.0);

        assert!(engine.halt_reason().is_none());
        engine.manual_halt();
        assert_eq!(engine.halt_reason(), Some(RiskBreach::ManualHalt));

        engine.reset_halt();
        assert!(engine.halt_reason().is_none());
    }
}

// ===========================================================================
// 4. Indicator Engine — full universe stress
// ===========================================================================

mod stress_indicator_engine {
    use super::*;
    use dhan_live_trader_common::tick_types::ParsedTick;
    use dhan_live_trader_trading::indicator::engine::IndicatorEngine;
    use dhan_live_trader_trading::indicator::types::IndicatorParams;

    fn make_tick(security_id: u32, ltp: f32, volume: u32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: 2,
            last_traded_price: ltp,
            volume,
            day_high: ltp * 1.01,
            day_low: ltp * 0.99,
            day_close: ltp,
            exchange_timestamp: 1772073900,
            ..Default::default()
        }
    }

    #[test]
    fn test_stress_indicator_engine_100k_ticks_1000_instruments() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        let start = Instant::now();

        for i in 0..100_000_u32 {
            let security_id = i % 1000;
            let ltp = 100.0 + (i as f32 * 0.01);
            let tick = make_tick(security_id, ltp, i + 1);
            let snapshot = engine.update(&tick);

            // No field should ever be NaN after valid input.
            assert!(
                snapshot.ema_fast.is_finite(),
                "ema_fast NaN at tick {i} for security {security_id}"
            );
            assert!(
                snapshot.ema_slow.is_finite(),
                "ema_slow NaN at tick {i} for security {security_id}"
            );
            assert!(
                snapshot.rsi.is_finite(),
                "rsi NaN at tick {i} for security {security_id}"
            );
            assert!(
                snapshot.macd_line.is_finite(),
                "macd_line NaN at tick {i} for security {security_id}"
            );
        }

        let elapsed = start.elapsed();
        if !skip_perf_assertions() {
            assert!(
                elapsed < Duration::from_secs(5),
                "100K ticks across 1000 instruments took {elapsed:?}"
            );
        }
    }

    #[test]
    fn test_stress_indicator_engine_out_of_bounds_security_id() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // Security ID beyond MAX_INDICATOR_INSTRUMENTS must return default snapshot.
        let tick = make_tick(u32::MAX, 100.0, 1);
        let snapshot = engine.update(&tick);

        assert_eq!(snapshot.security_id, u32::MAX);
        assert_eq!(snapshot.ema_fast, 0.0);
        assert!(!snapshot.is_warm);
    }

    #[test]
    fn test_stress_indicator_engine_zero_price_ticks() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        for _ in 0..100 {
            let tick = make_tick(1, 0.0, 0);
            let snapshot = engine.update(&tick);
            // Must not produce NaN even with zero inputs.
            assert!(snapshot.ema_fast.is_finite());
            assert!(snapshot.sma.is_finite());
        }
    }

    #[test]
    fn test_stress_indicator_engine_extreme_prices() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let extreme_prices = [
            0.01_f32,
            0.001,
            f32::MIN_POSITIVE,
            1.0,
            100.0,
            10_000.0,
            100_000.0,
            f32::MAX / 2.0,
        ];

        for &price in &extreme_prices {
            let tick = make_tick(42, price, 1);
            let snapshot = engine.update(&tick);
            assert!(
                snapshot.ema_fast.is_finite(),
                "ema_fast not finite for price {price}"
            );
            assert!(snapshot.rsi.is_finite(), "rsi not finite for price {price}");
        }
    }

    #[test]
    fn test_stress_indicator_engine_warmup_tracking() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // Feed exactly MAX_INDICATOR_WARMUP_TICKS ticks to one instrument.
        let warmup = dhan_live_trader_common::constants::MAX_INDICATOR_WARMUP_TICKS;
        for i in 0..warmup {
            let tick = make_tick(1, 100.0 + (i as f32), 100);
            let snapshot = engine.update(&tick);
            if i < warmup - 1 {
                assert!(!snapshot.is_warm, "must not be warm at tick {i}");
            }
        }

        let final_tick = make_tick(1, 200.0, 100);
        let snapshot = engine.update(&final_tick);
        assert!(snapshot.is_warm, "must be warm after {warmup} ticks");
    }

    #[test]
    fn test_stress_indicator_engine_vwap_monotonic_volume() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // VWAP with increasing volume should produce finite values.
        for i in 1..=1000_u32 {
            let tick = make_tick(1, 100.0, i * 100);
            let snapshot = engine.update(&tick);
            assert!(snapshot.vwap.is_finite(), "VWAP not finite at tick {i}");
        }
    }

    #[test]
    fn test_stress_indicator_engine_bollinger_convergence() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // Constant price: bollinger bands should converge to the price.
        let price = 500.0_f32;
        for _ in 0..200 {
            let tick = make_tick(1, price, 100);
            let _ = engine.update(&tick);
        }

        let tick = make_tick(1, price, 100);
        let snapshot = engine.update(&tick);
        assert!(
            (snapshot.bollinger_middle - f64::from(price)).abs() < 1.0,
            "bollinger middle should converge to price, got {}",
            snapshot.bollinger_middle
        );
    }

    #[test]
    fn test_stress_indicator_engine_rapid_price_swings() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // Oscillate price rapidly between 100 and 200.
        for i in 0..10_000_u32 {
            let price = if i % 2 == 0 { 100.0 } else { 200.0 };
            let tick = make_tick(1, price, 100);
            let snapshot = engine.update(&tick);
            assert!(snapshot.ema_fast.is_finite());
            assert!(snapshot.rsi.is_finite());
            assert!(snapshot.atr.is_finite());
            assert!(snapshot.supertrend.is_finite());
        }
    }
}

// ===========================================================================
// 5. State Machine — exhaustive transition matrix
// ===========================================================================

mod stress_state_machine {
    use dhan_live_trader_common::order_types::OrderStatus;
    use dhan_live_trader_trading::oms::state_machine::{is_valid_transition, parse_order_status};

    /// All 10 OrderStatus variants.
    const ALL_STATUSES: [OrderStatus; 10] = [
        OrderStatus::Transit,
        OrderStatus::Pending,
        OrderStatus::Confirmed,
        OrderStatus::PartTraded,
        OrderStatus::Traded,
        OrderStatus::Cancelled,
        OrderStatus::Rejected,
        OrderStatus::Expired,
        OrderStatus::Closed,
        OrderStatus::Triggered,
    ];

    #[test]
    fn test_stress_state_machine_exhaustive_all_100_transitions() {
        // Test all 10 x 10 = 100 transitions and count valid ones.
        let mut valid_count = 0;

        for &from in &ALL_STATUSES {
            for &to in &ALL_STATUSES {
                if is_valid_transition(from, to) {
                    valid_count += 1;
                }
            }
        }

        // The state machine has exactly 19 valid transitions implemented:
        // Transit->Pending, Transit->Rejected
        // Pending->Confirmed, Pending->PartTraded, Pending->Traded, Pending->Cancelled,
        // Pending->Rejected, Pending->Expired
        // Confirmed->PartTraded, Confirmed->Traded, Confirmed->Cancelled, Confirmed->Expired
        // PartTraded->Traded, PartTraded->Cancelled, PartTraded->Expired
        // Triggered->Pending, Triggered->Traded, Triggered->Cancelled, Triggered->Rejected
        assert_eq!(
            valid_count, 19,
            "expected 19 valid transitions in the DAG, found {valid_count}"
        );
    }

    #[test]
    fn test_stress_state_machine_terminal_states_block_all() {
        let terminal = [
            OrderStatus::Traded,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
            OrderStatus::Expired,
        ];

        for &from in &terminal {
            for &to in &ALL_STATUSES {
                assert!(
                    !is_valid_transition(from, to),
                    "terminal state {from:?} must block transition to {to:?}"
                );
            }
        }
    }

    #[test]
    fn test_stress_state_machine_no_self_transitions() {
        for &status in &ALL_STATUSES {
            assert!(
                !is_valid_transition(status, status),
                "self-transition {status:?} -> {status:?} must be invalid"
            );
        }
    }

    #[test]
    fn test_stress_state_machine_closed_blocks_all_outgoing() {
        for &to in &ALL_STATUSES {
            assert!(
                !is_valid_transition(OrderStatus::Closed, to),
                "Closed must block transition to {to:?}"
            );
        }
    }

    #[test]
    fn test_stress_state_machine_transit_only_two_outgoing() {
        let mut valid = vec![];
        for &to in &ALL_STATUSES {
            if is_valid_transition(OrderStatus::Transit, to) {
                valid.push(to);
            }
        }
        assert_eq!(
            valid.len(),
            2,
            "Transit should have exactly 2 outgoing transitions"
        );
        assert!(valid.contains(&OrderStatus::Pending));
        assert!(valid.contains(&OrderStatus::Rejected));
    }

    #[test]
    fn test_stress_state_machine_pending_six_outgoing() {
        let mut valid = vec![];
        for &to in &ALL_STATUSES {
            if is_valid_transition(OrderStatus::Pending, to) {
                valid.push(to);
            }
        }
        assert_eq!(valid.len(), 6, "Pending should have 6 outgoing transitions");
    }

    #[test]
    fn test_stress_state_machine_parse_roundtrip_all() {
        for &status in &ALL_STATUSES {
            let s = status.as_str();
            let parsed = parse_order_status(s);
            assert_eq!(parsed, Some(status), "roundtrip failed for {s}");
        }
    }

    #[test]
    fn test_stress_state_machine_parse_unknown_strings() {
        let unknown = [
            "", "INVALID", "traded", "pending", "unknown", "42", "NULL", "nil", "none", "ACTIVE",
            "FILLED",
        ];
        for &s in &unknown {
            assert!(
                parse_order_status(s).is_none(),
                "unknown string '{s}' must return None"
            );
        }
    }

    #[test]
    fn test_stress_state_machine_parse_aliases() {
        // Dhan uses these aliases in different contexts.
        assert_eq!(
            parse_order_status("PART_TRADED"),
            Some(OrderStatus::PartTraded)
        );
        assert_eq!(
            parse_order_status("PARTIALLY_FILLED"),
            Some(OrderStatus::PartTraded)
        );
        assert_eq!(
            parse_order_status("Cancelled"),
            Some(OrderStatus::Cancelled)
        );
        assert_eq!(
            parse_order_status("CANCELLED"),
            Some(OrderStatus::Cancelled)
        );
        assert_eq!(parse_order_status("CONFIRM"), Some(OrderStatus::Triggered));
        assert_eq!(
            parse_order_status("TRIGGERED"),
            Some(OrderStatus::Triggered)
        );
    }

    #[test]
    fn test_stress_state_machine_confirmed_four_outgoing() {
        let mut count = 0;
        for &to in &ALL_STATUSES {
            if is_valid_transition(OrderStatus::Confirmed, to) {
                count += 1;
            }
        }
        assert_eq!(count, 4, "Confirmed should have 4 outgoing transitions");
    }

    #[test]
    fn test_stress_state_machine_part_traded_three_outgoing() {
        let mut count = 0;
        for &to in &ALL_STATUSES {
            if is_valid_transition(OrderStatus::PartTraded, to) {
                count += 1;
            }
        }
        assert_eq!(count, 3, "PartTraded should have 3 outgoing transitions");
    }

    #[test]
    fn test_stress_state_machine_triggered_four_outgoing() {
        let mut count = 0;
        for &to in &ALL_STATUSES {
            if is_valid_transition(OrderStatus::Triggered, to) {
                count += 1;
            }
        }
        assert_eq!(count, 4, "Triggered should have 4 outgoing transitions");
    }
}

// ===========================================================================
// 6. Additional stress tests — cross-component
// ===========================================================================

mod stress_cross_component {
    use super::*;
    use dhan_live_trader_trading::indicator::types::{IndicatorParams, RingBuffer};
    use dhan_live_trader_trading::oms::circuit_breaker::OrderCircuitBreaker;
    use dhan_live_trader_trading::risk::engine::RiskEngine;

    #[test]
    fn test_stress_ring_buffer_10m_pushes() {
        let mut ring = RingBuffer::new();
        let start = Instant::now();

        for i in 0..10_000_000_u64 {
            let _ = ring.push(i as f64);
        }

        let elapsed = start.elapsed();
        assert!(!ring.is_empty());
        if !skip_perf_assertions() {
            assert!(
                elapsed < Duration::from_secs(2),
                "10M ring buffer pushes took {elapsed:?}"
            );
        }
    }

    #[test]
    fn test_stress_ring_buffer_eviction_correctness() {
        let mut ring = RingBuffer::new();
        let cap = dhan_live_trader_common::constants::INDICATOR_RING_BUFFER_CAPACITY;

        // Fill the buffer, then overwrite with known values.
        for i in 0..(cap * 3) {
            let evicted = ring.push(i as f64);
            if i >= cap {
                // After buffer is full, evicted values should be in order.
                assert_eq!(evicted, (i - cap) as f64, "eviction mismatch at {i}");
            }
        }
    }

    #[test]
    fn test_stress_ema_alpha_edge_periods() {
        // Very large period should produce small alpha (near zero but positive).
        let alpha = IndicatorParams::ema_alpha(u16::MAX);
        assert!(alpha > 0.0, "alpha must be positive");
        assert!(alpha < 0.001, "alpha with period u16::MAX should be tiny");

        // Period 1 should produce alpha = 1.0.
        let alpha_1 = IndicatorParams::ema_alpha(1);
        assert!((alpha_1 - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_stress_wilder_factor_edge_periods() {
        let factor = IndicatorParams::wilder_factor(u16::MAX);
        assert!(factor > 0.0);
        assert!(factor < 0.001);
    }

    #[test]
    fn test_stress_risk_engine_position_tracking_consistency() {
        let mut engine = RiskEngine::new(5.0, 100, 100_000_000.0);

        // Open and close 500 different positions.
        for sid in 50000..50500 {
            engine.record_fill(sid, 10, 100.0, 25);
            engine.update_market_price(sid, 105.0);
        }

        assert_eq!(engine.open_position_count(), 500);

        // Close all positions.
        for sid in 50000..50500 {
            engine.record_fill(sid, -10, 105.0, 25);
        }

        assert_eq!(engine.open_position_count(), 0);
    }

    #[test]
    fn test_stress_circuit_breaker_default_same_as_new() {
        let default_cb = OrderCircuitBreaker::default();
        let new_cb = OrderCircuitBreaker::new();

        assert_eq!(default_cb.state(), new_cb.state());
        assert!(default_cb.check().is_ok());
        assert!(new_cb.check().is_ok());
    }
}
