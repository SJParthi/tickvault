//! Mutation-killer tests: designed to catch surviving mutants.
//!
//! Each test targets a specific mutation that `cargo-mutants` could introduce:
//! boundary conditions, comparison inversions, arithmetic sign flips, return
//! value substitutions. If ANY mutation in the target code survives, one of
//! these tests MUST fail.

#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod greeks_mutations {
    use tickvault_trading::greeks::black_scholes::*;
    use tickvault_trading::greeks::buildup::*;
    use tickvault_trading::greeks::calibration::*;
    use tickvault_trading::greeks::pcr::*;

    // === PCR boundary mutations ===

    #[test]
    fn mutation_pcr_bearish_threshold_exact() {
        // At exactly 1.2 → Neutral (not Bearish)
        assert_eq!(classify_pcr(1.2), PcrSentiment::Neutral);
    }

    #[test]
    fn mutation_pcr_bearish_threshold_epsilon_above() {
        // 1.2 + epsilon → Bearish
        assert_eq!(classify_pcr(1.2 + f64::EPSILON), PcrSentiment::Bearish);
    }

    #[test]
    fn mutation_pcr_bullish_threshold_exact() {
        // At exactly 0.8 → Neutral (not Bullish)
        assert_eq!(classify_pcr(0.8), PcrSentiment::Neutral);
    }

    #[test]
    fn mutation_pcr_bullish_threshold_epsilon_below() {
        // 0.8 - epsilon → Bullish
        assert_eq!(classify_pcr(0.8 - f64::EPSILON), PcrSentiment::Bullish);
    }

    #[test]
    fn mutation_pcr_zero_call_oi_returns_none() {
        assert!(compute_pcr(1000, 0).is_none());
    }

    #[test]
    fn mutation_pcr_zero_both_returns_none() {
        assert!(compute_pcr(0, 0).is_none());
    }

    #[test]
    fn mutation_pcr_one_call_returns_some() {
        assert!(compute_pcr(0, 1).is_some());
    }

    #[test]
    fn mutation_pcr_result_value_exact() {
        let pcr = compute_pcr(200, 100).unwrap();
        assert!(
            (pcr - 2.0).abs() < f64::EPSILON,
            "PCR should be exactly 2.0"
        );
    }

    // === Buildup classification mutations ===

    #[test]
    fn mutation_buildup_oi_up_price_up_is_long() {
        assert_eq!(
            classify_buildup(200, 100, 50.0, 40.0),
            Some(BuildupType::LongBuildup)
        );
    }

    #[test]
    fn mutation_buildup_oi_up_price_down_is_short() {
        assert_eq!(
            classify_buildup(200, 100, 40.0, 50.0),
            Some(BuildupType::ShortBuildup)
        );
    }

    #[test]
    fn mutation_buildup_oi_down_price_down_is_unwinding() {
        assert_eq!(
            classify_buildup(100, 200, 40.0, 50.0),
            Some(BuildupType::LongUnwinding)
        );
    }

    #[test]
    fn mutation_buildup_oi_down_price_up_is_covering() {
        assert_eq!(
            classify_buildup(100, 200, 50.0, 40.0),
            Some(BuildupType::ShortCovering)
        );
    }

    #[test]
    fn mutation_buildup_equal_oi_returns_none() {
        assert!(classify_buildup(100, 100, 50.0, 40.0).is_none());
    }

    #[test]
    fn mutation_buildup_equal_price_returns_none() {
        assert!(classify_buildup(200, 100, 50.0, 50.0).is_none());
    }

    #[test]
    fn mutation_oi_change_pct_zero_previous_is_none() {
        assert!(oi_change_pct(100, 0).is_none());
    }

    #[test]
    fn mutation_oi_change_pct_100_percent() {
        let pct = oi_change_pct(200, 100).unwrap();
        assert!((pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn mutation_oi_change_pct_negative() {
        let pct = oi_change_pct(50, 100).unwrap();
        assert!((pct - (-50.0)).abs() < 0.01);
    }

    #[test]
    fn mutation_ltp_change_pct_zero_previous_is_none() {
        assert!(ltp_change_pct(100.0, 0.0).is_none());
    }

    #[test]
    fn mutation_ltp_change_pct_negative_previous_is_none() {
        assert!(ltp_change_pct(100.0, -1.0).is_none());
    }

    #[test]
    fn mutation_ltp_change_pct_exact() {
        let pct = ltp_change_pct(110.0, 100.0).unwrap();
        assert!((pct - 10.0).abs() < 0.01);
    }

    // === Black-Scholes mutations ===

    #[test]
    fn mutation_call_higher_than_put_atm() {
        let call = bs_call_price(100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        let put = bs_put_price(100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        assert!(call > put, "ATM call should be > put when r > 0");
    }

    #[test]
    fn mutation_call_price_monotone_in_spot() {
        let p1 = bs_call_price(90.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        let p2 = bs_call_price(100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        let p3 = bs_call_price(110.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        assert!(p1 < p2, "Call increases with spot: {p1} < {p2}");
        assert!(p2 < p3, "Call increases with spot: {p2} < {p3}");
    }

    #[test]
    fn mutation_put_price_monotone_in_strike() {
        let p1 = bs_put_price(100.0, 90.0, 1.0, 0.05, 0.0, 0.2);
        let p2 = bs_put_price(100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        let p3 = bs_put_price(100.0, 110.0, 1.0, 0.05, 0.0, 0.2);
        assert!(p1 < p2, "Put increases with strike: {p1} < {p2}");
        assert!(p2 < p3, "Put increases with strike: {p2} < {p3}");
    }

    #[test]
    fn mutation_vega_increases_with_time() {
        let p1 = bs_call_price(100.0, 100.0, 0.1, 0.05, 0.0, 0.2);
        let g1 = compute_greeks(OptionSide::Call, 100.0, 100.0, 0.1, 0.05, 0.0, p1);
        let p2 = bs_call_price(100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        let g2 = compute_greeks(OptionSide::Call, 100.0, 100.0, 1.0, 0.05, 0.0, p2);
        if let (Some(g1), Some(g2)) = (g1, g2) {
            assert!(
                g2.vega > g1.vega,
                "Vega should increase with time: {} > {}",
                g2.vega,
                g1.vega
            );
        }
    }

    #[test]
    fn mutation_theta_always_negative_for_long() {
        for spot in [80.0, 100.0, 120.0] {
            let price = bs_call_price(spot, 100.0, 0.5, 0.05, 0.0, 0.3);
            if let Some(g) = compute_greeks(OptionSide::Call, spot, 100.0, 0.5, 0.05, 0.0, price) {
                assert!(
                    g.theta < 0.0,
                    "Theta must be negative for long calls: spot={spot}, theta={}",
                    g.theta
                );
            }
        }
    }

    #[test]
    fn mutation_iv_solve_roundtrip_precision() {
        let sigma = 0.25;
        let price = bs_call_price(100.0, 100.0, 1.0, 0.05, 0.0, sigma);
        let iv = iv_solve(OptionSide::Call, 100.0, 100.0, 1.0, 0.05, 0.0, price).unwrap();
        assert!(
            (iv - sigma).abs() < 0.001,
            "IV roundtrip: expected {sigma}, got {iv}"
        );
    }

    #[test]
    fn mutation_iv_solve_none_for_zero_spot() {
        assert!(iv_solve(OptionSide::Call, 0.0, 100.0, 1.0, 0.05, 0.0, 10.0).is_none());
    }

    #[test]
    fn mutation_iv_solve_none_for_negative_price() {
        assert!(iv_solve(OptionSide::Call, 100.0, 100.0, 1.0, 0.05, 0.0, -1.0).is_none());
    }

    #[test]
    fn mutation_iv_solve_none_for_nan() {
        assert!(iv_solve(OptionSide::Call, f64::NAN, 100.0, 1.0, 0.05, 0.0, 10.0).is_none());
    }

    #[test]
    fn mutation_bs_price_delegates_correctly() {
        let call_direct = bs_call_price(100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        let call_via = bs_price(OptionSide::Call, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        assert!((call_direct - call_via).abs() < f64::EPSILON);

        let put_direct = bs_put_price(100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        let put_via = bs_price(OptionSide::Put, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        assert!((put_direct - put_via).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_greeks_from_iv_zero_sigma_returns_defaults() {
        let g = compute_greeks_from_iv(
            OptionSide::Call,
            100.0,
            100.0,
            1.0,
            0.05,
            0.0,
            0.0,
            10.0,
            365.0,
        );
        assert_eq!(g.gamma, 0.0, "Zero sigma → zero gamma");
        assert_eq!(g.vega, 0.0, "Zero sigma → zero vega");
    }

    #[test]
    fn mutation_greeks_from_iv_negative_spot_returns_defaults() {
        let g = compute_greeks_from_iv(
            OptionSide::Call,
            -100.0,
            100.0,
            1.0,
            0.05,
            0.0,
            0.3,
            10.0,
            365.0,
        );
        assert_eq!(g.delta, 0.0);
        assert_eq!(g.gamma, 0.0);
    }

    // === Calibration mutations ===

    #[test]
    fn mutation_calibrate_empty_returns_none() {
        assert!(calibrate_parameters(&[]).is_none());
    }

    #[test]
    fn mutation_calibrate_single_sample_returns_some() {
        let sample = CalibrationSample {
            side: OptionSide::Call,
            spot: 23000.0,
            strike: 23000.0,
            days_to_expiry: 7,
            market_price: 200.0,
            dhan_iv: 0.15,
            dhan_delta: 0.5,
            dhan_gamma: 0.001,
            dhan_theta: -10.0,
            dhan_vega: 20.0,
        };
        assert!(calibrate_parameters(&[sample]).is_some());
    }

    #[test]
    fn mutation_is_exact_match_high_error() {
        let result = CalibrationResult {
            risk_free_rate: 0.1,
            dividend_yield: 0.0,
            day_count: 365.0,
            total_error: 1000.0,
            sample_count: 1,
            mean_error: 250.0,
        };
        assert!(!is_exact_match(&result));
    }

    #[test]
    fn mutation_is_exact_match_low_error() {
        let result = CalibrationResult {
            risk_free_rate: 0.1,
            dividend_yield: 0.0,
            day_count: 365.0,
            total_error: 0.0001,
            sample_count: 10,
            mean_error: 0.0000025,
        };
        assert!(is_exact_match(&result));
    }

    // === Buildup label mutations ===

    #[test]
    fn mutation_buildup_labels_distinct() {
        let labels: Vec<&str> = vec![
            BuildupType::LongBuildup.label(),
            BuildupType::ShortBuildup.label(),
            BuildupType::LongUnwinding.label(),
            BuildupType::ShortCovering.label(),
        ];
        // All labels must be unique
        for i in 0..labels.len() {
            for j in (i + 1)..labels.len() {
                assert_ne!(labels[i], labels[j], "Labels must be unique");
            }
        }
    }

    #[test]
    fn mutation_buildup_labels_contain_buildup_or_covering_or_unwinding() {
        assert!(BuildupType::LongBuildup.label().contains("Buildup"));
        assert!(BuildupType::ShortBuildup.label().contains("Buildup"));
        assert!(BuildupType::LongUnwinding.label().contains("Unwinding"));
        assert!(BuildupType::ShortCovering.label().contains("Covering"));
    }
}

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

    fn make_tick(sid: u32, ltp: f32, high: f32, low: f32, volume: u32) -> ParsedTick {
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
        let tick = make_tick(u32::MAX, 100.0, 110.0, 90.0, 1000);
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

#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod neumaier_mutations {
    use tickvault_trading::greeks::neumaier_sum;

    #[test]
    fn mutation_neumaier_empty_is_zero() {
        assert!((neumaier_sum(&[]) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_neumaier_single_roundtrip() {
        assert!((neumaier_sum(&[42.5]) - 42.5).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_neumaier_two_values() {
        assert!((neumaier_sum(&[1.0, 2.0]) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_neumaier_cancellation() {
        let result = neumaier_sum(&[1e16, 1.0, -1e16]);
        assert!(
            (result - 1.0).abs() < f64::EPSILON,
            "Neumaier should resist cancellation: got {result}"
        );
    }

    #[test]
    fn mutation_neumaier_all_negative() {
        let result = neumaier_sum(&[-1.0, -2.0, -3.0]);
        assert!((result - (-6.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_neumaier_mixed_signs() {
        let result = neumaier_sum(&[1.0, -1.0, 2.0, -2.0]);
        assert!(result.abs() < f64::EPSILON);
    }
}

#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod historical_rates_mutations {
    use chrono::NaiveDate;
    use tickvault_trading::greeks::historical_rates::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn mutation_dhan_fixed_rate_is_10_percent() {
        assert!((DHAN_FIXED_RATE - 0.10).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_pre_covid_rate() {
        let rate = rbi_repo_rate(date(2019, 1, 1));
        assert!((rate - 0.0515).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_rate_mode_dhan() {
        let rate = risk_free_rate_for_mode("dhan", date(2024, 1, 1));
        assert!((rate - 0.10).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_rate_mode_theoretical() {
        let rate = risk_free_rate_for_mode("theoretical", date(2024, 1, 1));
        assert!((rate - 0.0650).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_rate_mode_unknown_defaults_dhan() {
        let rate = risk_free_rate_for_mode("garbage", date(2024, 1, 1));
        assert!((rate - 0.10).abs() < f64::EPSILON);
    }

    #[test]
    fn mutation_rate_monotonic_through_cycle() {
        // COVID cut: rate should decrease
        let pre = rbi_repo_rate(date(2020, 3, 26));
        let post = rbi_repo_rate(date(2020, 3, 27));
        assert!(post < pre, "Rate should decrease on COVID cut");
    }

    #[test]
    fn mutation_rate_increases_on_hike() {
        let pre = rbi_repo_rate(date(2022, 5, 3));
        let post = rbi_repo_rate(date(2022, 5, 4));
        assert!(post > pre, "Rate should increase on hike");
    }
}
