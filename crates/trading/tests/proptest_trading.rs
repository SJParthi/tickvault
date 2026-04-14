//! Property-based tests using proptest for mathematical invariants.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use proptest::prelude::*;

use tickvault_common::order_types::OrderStatus;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::greeks::black_scholes::*;
use tickvault_trading::greeks::buildup::*;
use tickvault_trading::greeks::pcr::*;
use tickvault_trading::indicator::engine::IndicatorEngine;
use tickvault_trading::indicator::obi::compute_obi;
use tickvault_trading::indicator::types::IndicatorParams;
use tickvault_trading::oms::state_machine::*;
use tickvault_trading::risk::engine::RiskEngine;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Creates a `ParsedTick` with the given LTP, high, low, close, and volume.
/// Uses `security_id = 1` by default (fits in `MAX_INDICATOR_INSTRUMENTS`).
fn make_tick(ltp: f32, high: f32, low: f32, close: f32, volume: u32) -> ParsedTick {
    ParsedTick {
        security_id: 1,
        exchange_segment_code: 1,
        last_traded_price: ltp,
        last_trade_quantity: 1,
        exchange_timestamp: 1_700_000_000,
        received_at_nanos: 0,
        average_traded_price: ltp,
        volume,
        total_sell_quantity: 0,
        total_buy_quantity: 0,
        day_open: ltp,
        day_close: close,
        day_high: high,
        day_low: low,
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

/// All terminal states in the order lifecycle.
const TERMINAL_STATES: [OrderStatus; 5] = [
    OrderStatus::Traded,
    OrderStatus::Cancelled,
    OrderStatus::Rejected,
    OrderStatus::Expired,
    OrderStatus::Closed,
];

/// All states in the order lifecycle.
const ALL_STATES: [OrderStatus; 10] = [
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

// ---------------------------------------------------------------------------
// 1. Black-Scholes: Put-call parity
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_put_call_parity(
        spot in 100.0_f64..50000.0,
        strike in 100.0_f64..50000.0,
        time in 0.003_f64..2.0,
        rate in 0.0_f64..0.15,
        div in 0.0_f64..0.05,
        sigma in 0.05_f64..2.0,
    ) {
        // Put-call parity: C - P = S*e^(-qT) - K*e^(-rT)
        let call = bs_call_price(spot, strike, time, rate, div, sigma);
        let put = bs_put_price(spot, strike, time, rate, div, sigma);
        let lhs = call - put;
        let rhs = spot * (-div * time).exp() - strike * (-rate * time).exp();
        let tol = (spot.max(strike)) * 1e-10;
        prop_assert!(
            (lhs - rhs).abs() < tol,
            "Put-call parity violated: C-P={}, S*e^(-qT)-K*e^(-rT)={}, diff={}",
            lhs, rhs, (lhs - rhs).abs()
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Black-Scholes: Call price bounds
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_call_price_bounds(
        spot in 100.0_f64..50000.0,
        strike in 100.0_f64..50000.0,
        time in 0.003_f64..2.0,
        rate in 0.0_f64..0.15,
        div in 0.0_f64..0.05,
        sigma in 0.05_f64..2.0,
    ) {
        let call = bs_call_price(spot, strike, time, rate, div, sigma);
        // Call price must be non-negative
        prop_assert!(call >= 0.0, "Call price negative: {}", call);
        // Call price must not exceed spot (discounted)
        let upper = spot * (-div * time).exp();
        prop_assert!(
            call <= upper + 1e-10,
            "Call {} exceeds discounted spot {}", call, upper
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Black-Scholes: Put price bounds
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_put_price_bounds(
        spot in 100.0_f64..50000.0,
        strike in 100.0_f64..50000.0,
        time in 0.003_f64..2.0,
        rate in 0.0_f64..0.15,
        div in 0.0_f64..0.05,
        sigma in 0.05_f64..2.0,
    ) {
        let put = bs_put_price(spot, strike, time, rate, div, sigma);
        // Put price must be non-negative
        prop_assert!(put >= 0.0, "Put price negative: {}", put);
        // Put price must not exceed discounted strike
        let upper = strike * (-rate * time).exp();
        prop_assert!(
            put <= upper + 1e-10,
            "Put {} exceeds discounted strike {}", put, upper
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Black-Scholes: IV solver roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_iv_roundtrip_call(
        spot in 500.0_f64..50000.0,
        strike in 500.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        sigma in 0.10_f64..1.0,
    ) {
        let price = bs_call_price(spot, strike, time, rate, div, sigma);
        // Skip near-zero and deep ITM calls where numerical precision degrades
        let intrinsic = (spot * (-div * time).exp() - strike * (-rate * time).exp()).max(0.0);
        if price < 0.01 || (intrinsic > 0.0 && (price - intrinsic) / price < 0.001) {
            return Ok(());
        }
        if let Some(recovered_iv) = iv_solve(OptionSide::Call, spot, strike, time, rate, div, price) {
            let recovered_price = bs_call_price(spot, strike, time, rate, div, recovered_iv);
            let tol = price * 1e-4 + 1e-6;
            prop_assert!(
                (recovered_price - price).abs() < tol,
                "IV roundtrip failed: orig_price={}, recovered_price={}, orig_sigma={}, recovered_iv={}",
                price, recovered_price, sigma, recovered_iv
            );
        }
        // iv_solve returning None is acceptable for edge cases
    }
}

// ---------------------------------------------------------------------------
// 5. Black-Scholes: IV solver roundtrip for puts
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_iv_roundtrip_put(
        spot in 500.0_f64..50000.0,
        strike in 500.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        sigma in 0.10_f64..1.0,
    ) {
        let price = bs_put_price(spot, strike, time, rate, div, sigma);
        // Skip near-zero and deep ITM puts where numerical precision degrades
        let intrinsic = (strike * (-rate * time).exp() - spot * (-div * time).exp()).max(0.0);
        if price < 0.01 || (intrinsic > 0.0 && (price - intrinsic) / price < 0.001) {
            return Ok(());
        }
        if let Some(recovered_iv) = iv_solve(OptionSide::Put, spot, strike, time, rate, div, price) {
            let recovered_price = bs_put_price(spot, strike, time, rate, div, recovered_iv);
            let tol = price * 1e-4 + 1e-6;
            prop_assert!(
                (recovered_price - price).abs() < tol,
                "IV roundtrip failed: orig_price={}, recovered_price={}, orig_sigma={}, recovered_iv={}",
                price, recovered_price, sigma, recovered_iv
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Black-Scholes: Greeks — call delta in [0, 1]
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_call_delta_bounds(
        spot in 500.0_f64..50000.0,
        strike in 500.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        sigma in 0.10_f64..1.5,
    ) {
        let price = bs_call_price(spot, strike, time, rate, div, sigma);
        if price < 0.01 {
            return Ok(());
        }
        if let Some(greeks) = compute_greeks(OptionSide::Call, spot, strike, time, rate, div, price) {
            prop_assert!(
                greeks.delta >= -1e-10 && greeks.delta <= 1.0 + 1e-10,
                "Call delta {} not in [0, 1]", greeks.delta
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Black-Scholes: Greeks — put delta in [-1, 0]
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_put_delta_bounds(
        spot in 500.0_f64..50000.0,
        strike in 500.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        sigma in 0.10_f64..1.5,
    ) {
        let price = bs_put_price(spot, strike, time, rate, div, sigma);
        if price < 0.01 {
            return Ok(());
        }
        if let Some(greeks) = compute_greeks(OptionSide::Put, spot, strike, time, rate, div, price) {
            prop_assert!(
                greeks.delta >= -1.0 - 1e-10 && greeks.delta <= 1e-10,
                "Put delta {} not in [-1, 0]", greeks.delta
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 8. Black-Scholes: Greeks — gamma is non-negative
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_gamma_non_negative(
        spot in 500.0_f64..50000.0,
        strike in 500.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        sigma in 0.10_f64..1.5,
    ) {
        let price = bs_call_price(spot, strike, time, rate, div, sigma);
        if price < 0.01 {
            return Ok(());
        }
        if let Some(greeks) = compute_greeks(OptionSide::Call, spot, strike, time, rate, div, price) {
            prop_assert!(
                greeks.gamma >= -1e-15,
                "Gamma {} is negative", greeks.gamma
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Black-Scholes: vega is non-negative
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_vega_non_negative(
        spot in 500.0_f64..50000.0,
        strike in 500.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        sigma in 0.10_f64..1.5,
    ) {
        let price = bs_call_price(spot, strike, time, rate, div, sigma);
        if price < 0.01 {
            return Ok(());
        }
        if let Some(greeks) = compute_greeks(OptionSide::Call, spot, strike, time, rate, div, price) {
            prop_assert!(
                greeks.vega >= -1e-15,
                "Vega {} is negative", greeks.vega
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 10. Black-Scholes: call-put gamma equality
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_call_put_gamma_equal(
        spot in 500.0_f64..50000.0,
        strike in 500.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        sigma in 0.10_f64..1.5,
    ) {
        let call_price = bs_call_price(spot, strike, time, rate, div, sigma);
        let put_price = bs_put_price(spot, strike, time, rate, div, sigma);
        if call_price < 0.01 || put_price < 0.01 {
            return Ok(());
        }
        let call_greeks = compute_greeks(OptionSide::Call, spot, strike, time, rate, div, call_price);
        let put_greeks = compute_greeks(OptionSide::Put, spot, strike, time, rate, div, put_price);
        if let (Some(cg), Some(pg)) = (call_greeks, put_greeks) {
            let tol = cg.gamma.abs() * 1e-4 + 1e-12;
            prop_assert!(
                (cg.gamma - pg.gamma).abs() < tol,
                "Call gamma {} != Put gamma {}", cg.gamma, pg.gamma
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 11. Black-Scholes: IV solver rejects invalid inputs
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_iv_solver_rejects_nonpositive_inputs(
        spot in -1000.0_f64..0.0,
        strike in 100.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        price in 1.0_f64..1000.0,
    ) {
        // Negative spot must return None
        let result = iv_solve(OptionSide::Call, spot, strike, time, rate, div, price);
        prop_assert!(result.is_none(), "IV solver should reject negative spot");
    }
}

// ---------------------------------------------------------------------------
// 12. PCR: always non-negative when defined
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_pcr_non_negative(
        put_oi in 0_u64..10_000_000,
        call_oi in 1_u64..10_000_000,
    ) {
        let pcr = compute_pcr(put_oi, call_oi);
        prop_assert!(pcr.is_some(), "PCR must be defined for non-zero call_oi");
        prop_assert!(pcr.unwrap() >= 0.0, "PCR must be >= 0, got {}", pcr.unwrap());
    }
}

// ---------------------------------------------------------------------------
// 13. PCR: classify_pcr consistency with thresholds
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_pcr_classify_consistency(
        pcr_value in 0.0_f64..5.0,
    ) {
        let sentiment = classify_pcr(pcr_value);
        match sentiment {
            PcrSentiment::Bullish => {
                prop_assert!(pcr_value < 0.8, "Bullish but PCR={}", pcr_value);
            }
            PcrSentiment::Neutral => {
                prop_assert!(
                    (0.8..=1.2).contains(&pcr_value),
                    "Neutral but PCR={}", pcr_value
                );
            }
            PcrSentiment::Bearish => {
                prop_assert!(pcr_value > 1.2, "Bearish but PCR={}", pcr_value);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 14. PCR: zero call OI returns None
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_pcr_zero_call_oi_returns_none(
        put_oi in 0_u64..10_000_000,
    ) {
        let pcr = compute_pcr(put_oi, 0);
        prop_assert!(pcr.is_none(), "PCR must be None when call_oi=0");
    }
}

// ---------------------------------------------------------------------------
// 15. Buildup: classify_buildup returns Some only when BOTH OI and price change
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_buildup_requires_both_changes(
        current_oi in 1_u32..10_000_000,
        previous_oi in 1_u32..10_000_000,
        current_price in 1.0_f64..100_000.0,
        previous_price in 1.0_f64..100_000.0,
    ) {
        let result = classify_buildup(current_oi, previous_oi, current_price, previous_price);
        if current_oi == previous_oi || (current_price - previous_price).abs() < f64::EPSILON {
            // If either OI or price is unchanged, should be None
            prop_assert!(result.is_none(), "Expected None when OI or price unchanged");
        } else {
            // Both changed -> should have a classification
            prop_assert!(result.is_some(), "Expected Some when both OI and price changed");
        }
    }
}

// ---------------------------------------------------------------------------
// 16. Buildup: oi_change_pct bounded when previous_oi > 0
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_buildup_oi_change_pct_bounded(
        current_oi in 0_u32..10_000_000,
        previous_oi in 1_u32..10_000_000,
    ) {
        let pct = oi_change_pct(current_oi, previous_oi);
        prop_assert!(pct.is_some(), "oi_change_pct must be defined for previous_oi > 0");
        let pct_val = pct.unwrap();
        // Lower bound: current_oi=0 gives -100%
        prop_assert!(pct_val >= -100.0, "OI change pct {} below -100%", pct_val);
        // Must be finite
        prop_assert!(pct_val.is_finite(), "OI change pct must be finite");
    }
}

// ---------------------------------------------------------------------------
// 17. Buildup: quadrant correctness
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_buildup_quadrant_correct(
        oi_delta in 1_u32..5_000_000,
        price_delta in 0.01_f64..5000.0,
        base_oi in 1_u32..5_000_000,
        base_price in 1.0_f64..50_000.0,
    ) {
        // Long buildup: OI up, price up
        let lb = classify_buildup(base_oi + oi_delta, base_oi, base_price + price_delta, base_price);
        prop_assert_eq!(lb, Some(BuildupType::LongBuildup));

        // Short buildup: OI up, price down
        let sb = classify_buildup(base_oi + oi_delta, base_oi, base_price - price_delta.min(base_price - 0.01), base_price);
        if base_price - price_delta.min(base_price - 0.01) < base_price {
            prop_assert_eq!(sb, Some(BuildupType::ShortBuildup));
        }

        // Short covering: OI down, price up
        let sc = classify_buildup(base_oi, base_oi + oi_delta, base_price + price_delta, base_price);
        prop_assert_eq!(sc, Some(BuildupType::ShortCovering));

        // Long unwinding: OI down, price down
        let lu = classify_buildup(base_oi, base_oi + oi_delta, base_price - price_delta.min(base_price - 0.01), base_price);
        if base_price - price_delta.min(base_price - 0.01) < base_price {
            prop_assert_eq!(lu, Some(BuildupType::LongUnwinding));
        }
    }
}

// ---------------------------------------------------------------------------
// 18. Risk Engine: halt is permanent until reset
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_risk_halt_permanent(
        num_orders in 1_u32..50,
        order_lots in 1_i32..10,
    ) {
        let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
        engine.manual_halt();

        // All orders after halt must be rejected
        for _ in 0..num_orders {
            let check = engine.check_order(1001, order_lots);
            prop_assert!(!check.is_approved(), "Order approved despite halt");
        }

        // After reset, orders should be approved again
        engine.reset_halt();
        let check = engine.check_order(1001, order_lots);
        prop_assert!(check.is_approved(), "Order rejected after halt reset");
    }
}

// ---------------------------------------------------------------------------
// 19. Risk Engine: position tracking consistency (buy + sell = net)
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_risk_position_tracking(
        buy_lots in 1_i32..50,
        sell_lots in 1_i32..50,
        price in 100.0_f64..50000.0,
    ) {
        let mut engine = RiskEngine::new(2.0, 200, 10_000_000.0);

        // Buy
        engine.record_fill(1001, buy_lots, price, 25);
        let pos = engine.position(1001).unwrap();
        prop_assert_eq!(pos.net_lots, buy_lots, "After buy: net_lots mismatch");

        // Sell
        engine.record_fill(1001, -sell_lots, price, 25);
        let pos = engine.position(1001).unwrap();
        prop_assert_eq!(
            pos.net_lots,
            buy_lots - sell_lots,
            "After sell: net_lots mismatch (buy={}, sell={})",
            buy_lots, sell_lots
        );
    }
}

// ---------------------------------------------------------------------------
// 20. Risk Engine: daily loss enforcement
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_risk_daily_loss_halts(
        capital in 100_000.0_f64..10_000_000.0,
        max_loss_pct in 0.5_f64..5.0,
    ) {
        let mut engine = RiskEngine::new(max_loss_pct, 1000, capital);
        let max_loss = capital * (max_loss_pct / 100.0);

        // Record a loss that exceeds the threshold
        // Buy at high price, mark at low price to create unrealized loss
        engine.record_fill(1001, 1, max_loss * 2.0, 1);
        engine.update_market_price(1001, 0.01);

        let check = engine.check_order(1002, 1);
        prop_assert!(!check.is_approved(), "Order approved despite exceeding max daily loss");
        prop_assert!(engine.is_halted(), "Engine not halted after daily loss breach");
    }
}

// ---------------------------------------------------------------------------
// 21. Indicator Engine: RSI in [0, 100] after warmup
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_indicator_rsi_bounded(
        prices in proptest::collection::vec(10.0_f32..50000.0, 40..100),
    ) {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let mut last_snapshot = None;
        for &price in &prices {
            let tick = make_tick(price, price * 1.01, price * 0.99, price, 1000);
            last_snapshot = Some(engine.update(&tick));
        }

        if let Some(snap) = last_snapshot
            && snap.is_warm
        {
            prop_assert!(
                snap.rsi >= 0.0 && snap.rsi <= 100.0,
                "RSI {} out of [0, 100] bounds", snap.rsi
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 22. Indicator Engine: EMA bounded by min/max of inputs
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_indicator_ema_bounded(
        prices in proptest::collection::vec(10.0_f32..50000.0, 40..100),
    ) {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let min_price = prices.iter().copied().fold(f32::INFINITY, f32::min);
        let max_price = prices.iter().copied().fold(f32::NEG_INFINITY, f32::max);

        let mut last_snapshot = None;
        for &price in &prices {
            let tick = make_tick(price, price * 1.01, price * 0.99, price, 1000);
            last_snapshot = Some(engine.update(&tick));
        }

        if let Some(snap) = last_snapshot {
            // EMA fast must be within the price range
            prop_assert!(
                snap.ema_fast >= f64::from(min_price) - 1e-6
                    && snap.ema_fast <= f64::from(max_price) + 1e-6,
                "EMA fast {} not in [{}, {}]", snap.ema_fast, min_price, max_price
            );
            // EMA slow must also be within the price range
            prop_assert!(
                snap.ema_slow >= f64::from(min_price) - 1e-6
                    && snap.ema_slow <= f64::from(max_price) + 1e-6,
                "EMA slow {} not in [{}, {}]", snap.ema_slow, min_price, max_price
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 23. Indicator Engine: SMA of constant input within SMA period window
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_indicator_sma_constant_within_period(
        constant_price in 10.0_f32..50000.0,
        // SMA period is 20 by default. Within the first 20 ticks the running sum
        // is built from real data only (no ring buffer eviction).
        num_ticks in 1_usize..21,
    ) {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let mut last_snapshot = None;
        for _ in 0..num_ticks {
            let tick = make_tick(
                constant_price,
                constant_price,
                constant_price,
                constant_price,
                1000,
            );
            last_snapshot = Some(engine.update(&tick));
        }

        if let Some(snap) = last_snapshot {
            let expected = f64::from(constant_price);
            let tol = expected * 1e-6 + 1e-6;
            prop_assert!(
                (snap.sma - expected).abs() < tol,
                "SMA {} != constant price {} (diff={}) after {} ticks",
                snap.sma, expected, (snap.sma - expected).abs(), num_ticks
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 24. State Machine: terminal states have no valid outgoing transitions
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_state_machine_terminal_no_outgoing(
        terminal_idx in 0_usize..5,
        target_idx in 0_usize..10,
    ) {
        let terminal = TERMINAL_STATES[terminal_idx];
        let target = ALL_STATES[target_idx];
        prop_assert!(
            !is_valid_transition(terminal, target),
            "Terminal state {:?} should not transition to {:?}",
            terminal, target
        );
    }
}

// ---------------------------------------------------------------------------
// 25. State Machine: self-transitions are always invalid
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_state_machine_no_self_transition(
        state_idx in 0_usize..10,
    ) {
        let state = ALL_STATES[state_idx];
        prop_assert!(
            !is_valid_transition(state, state),
            "Self-transition {:?} -> {:?} should be invalid",
            state, state
        );
    }
}

// ---------------------------------------------------------------------------
// 26. State Machine: parse_order_status roundtrip for known statuses
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_state_machine_parse_roundtrip(
        state_idx in 0_usize..10,
    ) {
        let state = ALL_STATES[state_idx];
        let s = state.as_str();
        let parsed = parse_order_status(s);
        prop_assert_eq!(
            parsed,
            Some(state),
            "Roundtrip failed for {:?}: as_str()={:?}, parsed={:?}",
            state, s, parsed
        );
    }
}

// ---------------------------------------------------------------------------
// 27. Black-Scholes: higher volatility -> higher call price
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_bs_call_monotone_in_volatility(
        spot in 500.0_f64..50000.0,
        strike in 500.0_f64..50000.0,
        time in 0.01_f64..1.0,
        rate in 0.01_f64..0.12,
        div in 0.0_f64..0.03,
        sigma_low in 0.05_f64..0.75,
        sigma_extra in 0.01_f64..0.75,
    ) {
        let sigma_high = sigma_low + sigma_extra;
        let call_low = bs_call_price(spot, strike, time, rate, div, sigma_low);
        let call_high = bs_call_price(spot, strike, time, rate, div, sigma_high);
        prop_assert!(
            call_high >= call_low - 1e-10,
            "Call not monotone in vol: C(sigma={})={} < C(sigma={})={}",
            sigma_high, call_high, sigma_low, call_low
        );
    }
}

// ---------------------------------------------------------------------------
// 28. Indicator Engine: Bollinger Band ordering
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_indicator_bollinger_ordering(
        prices in proptest::collection::vec(10.0_f32..50000.0, 40..100),
    ) {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let mut last_snapshot = None;
        for &price in &prices {
            let tick = make_tick(price, price * 1.01, price * 0.99, price, 1000);
            last_snapshot = Some(engine.update(&tick));
        }

        if let Some(snap) = last_snapshot
            && snap.is_warm
        {
            prop_assert!(
                snap.bollinger_lower <= snap.bollinger_middle,
                "BB lower {} > middle {}", snap.bollinger_lower, snap.bollinger_middle
            );
            prop_assert!(
                snap.bollinger_middle <= snap.bollinger_upper,
                "BB middle {} > upper {}", snap.bollinger_middle, snap.bollinger_upper
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 29. OBI: always bounded in [-1, +1]
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_obi_bounded(
        bid_prices in proptest::collection::vec(0.01_f64..100000.0, 1..20),
        bid_qtys in proptest::collection::vec(1_u32..1000000, 1..20),
        ask_prices in proptest::collection::vec(0.01_f64..100000.0, 1..20),
        ask_qtys in proptest::collection::vec(1_u32..1000000, 1..20),
    ) {
        use tickvault_common::tick_types::DeepDepthLevel;

        let bid_count = bid_prices.len().min(bid_qtys.len());
        let ask_count = ask_prices.len().min(ask_qtys.len());

        let bids: Vec<DeepDepthLevel> = (0..bid_count)
            .map(|i| DeepDepthLevel { price: bid_prices[i], quantity: bid_qtys[i], orders: 1 })
            .collect();
        let asks: Vec<DeepDepthLevel> = (0..ask_count)
            .map(|i| DeepDepthLevel { price: ask_prices[i], quantity: ask_qtys[i], orders: 1 })
            .collect();

        let snap = compute_obi(1, 2, &bids, &asks);

        prop_assert!(snap.obi >= -1.0 && snap.obi <= 1.0,
            "OBI {} out of [-1, +1]", snap.obi);
        prop_assert!(snap.weighted_obi >= -1.0 && snap.weighted_obi <= 1.0,
            "Weighted OBI {} out of [-1, +1]", snap.weighted_obi);
        prop_assert!(snap.spread >= 0.0 || snap.spread.is_finite(),
            "Spread must be finite: {}", snap.spread);
    }
}

// ---------------------------------------------------------------------------
// 30. OBI: total quantities match sum of individual levels
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_obi_quantity_conservation(
        qtys in proptest::collection::vec(0_u32..100000, 1..20),
    ) {
        use tickvault_common::tick_types::DeepDepthLevel;

        let levels: Vec<DeepDepthLevel> = qtys.iter()
            .enumerate()
            .map(|(i, &q)| DeepDepthLevel { price: 100.0 + i as f64, quantity: q, orders: 1 })
            .collect();

        let snap = compute_obi(1, 2, &levels, &[]);

        let expected_total: u64 = qtys.iter()
            .filter(|&&q| q > 0)
            .map(|&q| u64::from(q))
            .sum();

        prop_assert_eq!(snap.total_bid_qty, expected_total,
            "Total bid qty mismatch: computed={}, expected={}", snap.total_bid_qty, expected_total);
    }
}
