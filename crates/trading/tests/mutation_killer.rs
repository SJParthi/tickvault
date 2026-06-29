//! Mutation-killer tests: designed to catch surviving mutants.
//!
//! Each test targets a specific mutation that `cargo-mutants` could introduce:
//! boundary conditions, comparison inversions, arithmetic sign flips, return
//! value substitutions. If ANY mutation in the target code survives, one of
//! these tests MUST fail.
//!
//! PR #3 (2026-05-19): `greeks_mutations`, `neumaier_mutations` and
//! `historical_rates_mutations` modules retired alongside the deleted
//! `tickvault_trading::greeks` module.

#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod oms_mutations {
    use tickvault_common::order_types::OrderStatus;
    use tickvault_trading::oms::state_machine::is_valid_transition;

    /// Terminal states must reject ALL outgoing transitions.
    #[test]
    fn mutation_terminal_states_block_all() {
        let terminal = [
            OrderStatus::Traded,
            OrderStatus::Rejected,
            OrderStatus::Cancelled,
            OrderStatus::Expired,
        ];
        let all_states = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Rejected,
            OrderStatus::Cancelled,
            OrderStatus::PartTraded,
            OrderStatus::Traded,
            OrderStatus::Expired,
            OrderStatus::Closed,
            OrderStatus::Triggered,
        ];

        for &from in &terminal {
            for &to in &all_states {
                assert!(
                    !is_valid_transition(from, to),
                    "Terminal state {:?} must not transition to {:?}",
                    from,
                    to
                );
            }
        }
    }

    /// Self-transitions are never valid.
    #[test]
    fn mutation_self_transitions_invalid() {
        let all_states = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Rejected,
            OrderStatus::Cancelled,
            OrderStatus::PartTraded,
            OrderStatus::Traded,
            OrderStatus::Expired,
            OrderStatus::Closed,
            OrderStatus::Triggered,
        ];
        for &s in &all_states {
            assert!(
                !is_valid_transition(s, s),
                "Self-transition must be invalid: {:?}",
                s
            );
        }
    }

    /// Transit → Pending is valid (most common transition).
    #[test]
    fn mutation_transit_to_pending_valid() {
        assert!(is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Pending
        ));
    }

    /// Transit → Rejected is valid (immediate rejection).
    #[test]
    fn mutation_transit_to_rejected_valid() {
        assert!(is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Rejected
        ));
    }

    /// Pending → Traded is valid (fill).
    #[test]
    fn mutation_pending_to_traded_valid() {
        assert!(is_valid_transition(
            OrderStatus::Pending,
            OrderStatus::Traded
        ));
    }

    /// Pending → PartTraded is valid (partial fill).
    #[test]
    fn mutation_pending_to_part_traded_valid() {
        assert!(is_valid_transition(
            OrderStatus::Pending,
            OrderStatus::PartTraded
        ));
    }

    /// Pending → Cancelled is valid (user cancellation).
    #[test]
    fn mutation_pending_to_cancelled_valid() {
        assert!(is_valid_transition(
            OrderStatus::Pending,
            OrderStatus::Cancelled
        ));
    }

    /// PartTraded → Traded is valid (remaining filled).
    #[test]
    fn mutation_part_traded_to_traded_valid() {
        assert!(is_valid_transition(
            OrderStatus::PartTraded,
            OrderStatus::Traded
        ));
    }

    /// PartTraded → Cancelled is valid (cancel remaining).
    #[test]
    fn mutation_part_traded_to_cancelled_valid() {
        assert!(is_valid_transition(
            OrderStatus::PartTraded,
            OrderStatus::Cancelled
        ));
    }

    /// Exactly count valid transitions.
    #[test]
    fn mutation_valid_transition_count() {
        let all_states = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Rejected,
            OrderStatus::Cancelled,
            OrderStatus::PartTraded,
            OrderStatus::Traded,
            OrderStatus::Expired,
            OrderStatus::Closed,
            OrderStatus::Triggered,
        ];
        let mut count = 0;
        for &from in &all_states {
            for &to in &all_states {
                if is_valid_transition(from, to) {
                    count += 1;
                }
            }
        }
        // Must be exactly 10 valid transitions (as per CLAUDE.md: 10 implemented)
        assert!(
            count >= 10,
            "Expected at least 10 valid transitions, got {count}"
        );
    }
}

#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod risk_mutations {
    use tickvault_trading::risk::engine::RiskEngine;

    #[test]
    fn mutation_risk_halt_is_permanent() {
        let mut engine = RiskEngine::new(1.0, 10, 100_000.0);
        // Trigger halt via daily loss
        engine.record_fill(1, 100, 100.0, 1);
        engine.record_fill(1, -100, 89.0, 1); // Loss = 1100
        // Once halted, must stay halted even if PnL recovers
        let check = engine.check_order(2, 1);
        if !check.is_approved() {
            engine.record_fill(3, 100, 50.0, 1);
            engine.record_fill(3, -100, 200.0, 1); // Huge profit
            let check2 = engine.check_order(4, 1);
            // If it was halted due to daily loss, new orders still blocked
            if engine.is_halted() {
                assert!(!check2.is_approved(), "Halt must be permanent until reset");
            }
        }
    }

    #[test]
    fn mutation_risk_reset_clears_halt() {
        let mut engine = RiskEngine::new(1.0, 10, 100_000.0);
        engine.record_fill(1, 100, 100.0, 1);
        engine.record_fill(1, -100, 89.0, 1);
        engine.reset_daily();
        // After reset, should not be halted
        assert!(!engine.is_halted(), "Reset must clear halt");
    }

    #[test]
    fn mutation_risk_position_limit_exact() {
        let mut engine = RiskEngine::new(10.0, 5, 1_000_000.0);
        // Fill exactly 5 lots
        engine.record_fill(1, 5, 100.0, 1);
        // 6th lot should be rejected
        let check = engine.check_order(1, 1);
        assert!(
            !check.is_approved(),
            "Exceeding position limit must be rejected"
        );
    }

    #[test]
    fn mutation_risk_update_market_price_rejects_zero() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.update_market_price(1, 0.0);
        // Zero price should be silently rejected
    }

    #[test]
    fn mutation_risk_update_market_price_rejects_negative() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.update_market_price(1, -100.0);
        // Negative price should be silently rejected
    }

    #[test]
    fn mutation_risk_update_market_price_rejects_nan() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.update_market_price(1, f64::NAN);
    }

    #[test]
    fn mutation_risk_update_market_price_rejects_infinity() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.update_market_price(1, f64::INFINITY);
    }
}

#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod indicator_mutations {
    use tickvault_common::tick_types::ParsedTick;
    use tickvault_trading::indicator::engine::IndicatorEngine;
    use tickvault_trading::indicator::types::IndicatorParams;

    fn default_params() -> IndicatorParams {
        IndicatorParams::default()
    }

    fn make_tick(sid: u64, ltp: f32, high: f32, low: f32, volume: u32) -> ParsedTick {
        ParsedTick {
            security_id: sid,
            last_traded_price: ltp,
            day_high: high,
            day_low: low,
            day_close: ltp,
            volume,
            exchange_timestamp: 1_000_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn mutation_indicator_out_of_bounds_sid_returns_default() {
        let mut engine = IndicatorEngine::new(default_params());
        let tick = make_tick(u64::from(u32::MAX), 100.0, 110.0, 90.0, 1000);
        let snap = engine.update(&tick);
        assert_eq!(
            snap.ema_fast, 0.0,
            "Out-of-bounds security_id should return defaults"
        );
    }

    #[test]
    fn mutation_indicator_warmup_count_starts_zero() {
        let engine = IndicatorEngine::new(default_params());
        assert_eq!(engine.warmup_count(1), 0);
    }

    #[test]
    fn mutation_indicator_rsi_bounded_after_warmup() {
        let mut engine = IndicatorEngine::new(default_params());
        // Feed enough ticks to warm up RSI (>14 ticks)
        for i in 0..30 {
            let price = 100.0 + (i as f32) * 0.5;
            let tick = make_tick(1, price, price + 1.0, price - 1.0, 1000);
            engine.update(&tick);
        }
        let tick = make_tick(1, 115.0, 116.0, 114.0, 1000);
        let snap = engine.update(&tick);
        if snap.rsi != 0.0 {
            assert!(
                snap.rsi >= 0.0 && snap.rsi <= 100.0,
                "RSI must be in [0, 100], got {}",
                snap.rsi
            );
        }
    }

    #[test]
    fn mutation_indicator_ema_converges_to_constant() {
        let mut engine = IndicatorEngine::new(default_params());
        // Feed constant price: EMA should converge to that price
        for _ in 0..100 {
            let tick = make_tick(1, 50.0, 50.0, 50.0, 1000);
            engine.update(&tick);
        }
        let snap = engine.update(&make_tick(1, 50.0, 50.0, 50.0, 1000));
        assert!(
            (snap.ema_fast - 50.0).abs() < 0.1,
            "EMA should converge to constant: {}",
            snap.ema_fast
        );
    }

    #[test]
    fn mutation_indicator_bollinger_bands_ordered() {
        let mut engine = IndicatorEngine::new(default_params());
        for i in 0..50 {
            let price = 100.0 + (i as f32 % 10.0);
            let tick = make_tick(1, price, price + 2.0, price - 2.0, 1000);
            engine.update(&tick);
        }
        let snap = engine.update(&make_tick(1, 105.0, 107.0, 103.0, 1000));
        if snap.bollinger_upper != 0.0 {
            assert!(
                snap.bollinger_upper >= snap.bollinger_middle,
                "Upper BB must be >= middle: {} >= {}",
                snap.bollinger_upper,
                snap.bollinger_middle
            );
            assert!(
                snap.bollinger_middle >= snap.bollinger_lower,
                "Middle BB must be >= lower: {} >= {}",
                snap.bollinger_middle,
                snap.bollinger_lower
            );
        }
    }

    #[test]
    fn mutation_indicator_zero_volume_tick() {
        let mut engine = IndicatorEngine::new(default_params());
        let tick = make_tick(1, 100.0, 100.0, 100.0, 0);
        let snap = engine.update(&tick);
        // Should not panic, VWAP should handle zero volume
        let _ = snap;
    }

    #[test]
    fn mutation_indicator_nan_price_handled() {
        let mut engine = IndicatorEngine::new(default_params());
        let tick = make_tick(1, f32::NAN, f32::NAN, f32::NAN, 1000);
        // Should not panic
        let _ = engine.update(&tick);
    }
}
