//! Parity oracle: exercises the bruteX `trading-core` git dependency
//! (pinned rev, dev-only) so the pin is actually COMPILED and its documented
//! numeric contracts hold. Read-only consumption — no bruteX code is copied
//! into tickvault (`.claude/rules/project/brutex-readonly-lock-2026-07-18.md`).
//!
//! Inputs are chosen so every expected retracement/pivot lands on an exact
//! integer paisa, independent of the crate's internal rounding mode.

use trading_core::indicators::round_half_even;
use trading_core::{compute_fib_levels, compute_pivot_levels, greeks};

#[test]
fn test_fib_levels_match_hand_derived_values() {
    // PDH ₹110.00, PDL ₹90.00 (integer paisa).
    let fib = compute_fib_levels(11_000, 9_000);
    assert_eq!(fib.pdh_paisa, 11_000);
    assert_eq!(fib.pdl_paisa, 9_000);
    assert_eq!(fib.range_paisa, 2_000);
    assert_eq!(fib.retrace_50_paisa, 10_000);
    // Bearish retracements (PDH-anchored): PDH - ratio * range.
    assert_eq!(fib.retrace_23_paisa, 10_528); // 11000 - 0.236 * 2000
    assert_eq!(fib.retrace_38_paisa, 10_236); // 11000 - 0.382 * 2000
    assert_eq!(fib.retrace_62_paisa, 9_764); // 11000 - 0.618 * 2000
    assert_eq!(fib.retrace_78_paisa, 9_428); // 11000 - 0.786 * 2000
    // Bullish retracements (PDL-anchored): PDL + ratio * range.
    assert_eq!(fib.retrace_bull_23_paisa, 9_472); // 9000 + 0.236 * 2000
    assert_eq!(fib.retrace_bull_78_paisa, 10_572); // 9000 + 0.786 * 2000
}

#[test]
fn test_pivot_levels_match_classic_floor_formulas() {
    // H ₹110.00, L ₹90.00, C ₹100.00 — exact integer pivots.
    let piv = compute_pivot_levels(11_000, 9_000, 10_000);
    assert_eq!(piv.pdh_paisa, 11_000);
    assert_eq!(piv.pdl_paisa, 9_000);
    assert_eq!(piv.pdc_paisa, 10_000);
    assert_eq!(piv.p_paisa, 10_000); // (H + L + C) / 3
    assert_eq!(piv.r1_paisa, 11_000); // 2P - L
    assert_eq!(piv.s1_paisa, 9_000); // 2P - H
    assert_eq!(piv.r2_paisa, 12_000); // P + (H - L)
    assert_eq!(piv.s2_paisa, 8_000); // P - (H - L)
}

#[test]
fn test_black76_greeks_satisfy_put_call_parity() {
    // F = 100, K = 95, T = 0.5y, r = 6%, sigma = 25% — sigma supplied,
    // model price computed. Parity is formula-independent:
    // C - P = e^(-rT) * (F - K).
    let call = greeks(None, Some(0.25), 100.0, 95.0, 0.5, 0.06, true)
        .expect("call greeks with supplied sigma");
    let put = greeks(None, Some(0.25), 100.0, 95.0, 0.5, 0.06, false)
        .expect("put greeks with supplied sigma");
    let expected = (-0.06_f64 * 0.5).exp() * (100.0 - 95.0);
    let gap = (call.price - put.price) - expected;
    assert!(gap.abs() < 1e-9, "put-call parity violated: gap={gap}");
    // Documented sign/range contracts.
    assert!(
        call.delta > 0.0 && call.delta <= 1.0,
        "CE delta must sit in (0, 1]: {}",
        call.delta
    );
    assert!(
        put.delta < 0.0 && put.delta >= -1.0,
        "PE delta must sit in [-1, 0): {}",
        put.delta
    );
    assert!(
        call.gamma >= 0.0 && put.gamma >= 0.0,
        "gamma is non-negative"
    );
    assert!(call.vega >= 0.0 && put.vega >= 0.0, "vega is non-negative");
    assert!(
        (call.iv - 0.25).abs() < 1e-9,
        "supplied sigma must be echoed as iv: {}",
        call.iv
    );
}

#[test]
fn test_round_half_even_matches_bankers_rounding() {
    assert_eq!(round_half_even(2.5), 2);
    assert_eq!(round_half_even(3.5), 4);
    assert_eq!(round_half_even(-2.5), -2);
    assert_eq!(round_half_even(10.0), 10);
}
